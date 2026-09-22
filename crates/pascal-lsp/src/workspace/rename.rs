//! Complete, isolated workspace snapshots used by rename and code actions.
//!
//! Navigation intentionally loads a small dependency closure.  Rename cannot
//! use that closure: an unopened reverse consumer may be anywhere in the
//! selected workspace.  This module therefore builds a bounded, throw-away
//! index for each expensive request and never mutates the live navigation
//! cache or the filesystem.

use super::resolver as shared_resolver;
use super::{
    ContextKey, ContextState, DiskStamp, KnownDocumentOwner, OpenDocument, PathStamp, Workspace,
    WorkspaceOptions, absolute_path, canonical_file_uri, disk_stamp, is_analyzable_source_path,
    is_configuration_file, is_pascal_path, path_stamp, path_stamp_result, path_starts_with_ci,
    path_starts_with_native, paths_equal_ci, read_disk_source, read_disk_source_with_cancel,
};
use crate::NavigationIndex;
use crate::include_expansion::{
    self, ExpansionLimits, ExpansionResult, IncludeObservation as ExpansionIncludeObservation,
    IncludeResolver, ResolvedInclude,
};
use crate::navigation::AssistanceBudget;
use crate::navigation::ParsedDocument;
use crate::text;
use lsp_types::{
    DocumentChanges, DocumentHighlight, Location, OneOf, OptionalVersionedTextDocumentIdentifier,
    Position, PrepareRenameResponse, Range, TextDocumentEdit, TextEdit, Url, WorkspaceEdit,
};
use pascal_core::conditional::{
    self, ConditionalDirective, DirectiveKind as ConditionalDirectiveKind,
};
use pascal_core::decode_bytes;
use pascal_core::resolver::{
    IncludeResolveRequest, LegacyRoute, LoadedSource, Resolution, ResolverLimits,
};
use pascal_project::delphi_overrides::EffectiveOverrides;
use pascal_project::{
    ConditionalContext, MetadataObservation, ProjectCandidateMembership, ProjectContext,
    ProjectPathEntry, ProjectPathProvenance, ProjectSelections, ReadPolicy,
    has_invalid_project_selection,
};
#[cfg(test)]
use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::{self, Read};
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use walkdir::WalkDir;

#[cfg(test)]
use std::sync::mpsc::{Receiver, Sender};
#[cfg(test)]
use std::sync::{Mutex, OnceLock};

pub(crate) const CANCELLATION_MESSAGE: &str = "request cancelled";
const MAX_SNAPSHOT_DEPENDENCY_FILES: usize = 512;
// Discovery limits are deliberately independent from the retained analysis
// limits. The multidev workspace has more than 436,000 filesystem entries and
// more than 10,000 Pascal sources, while maxFiles/maxTotalBytes describe the
// parsed/indexed working set rather than the directory walk.
const MAX_RENAME_TRAVERSAL_ENTRIES: usize = 1_048_576;
const MAX_RENAME_SCANNED_BYTES: usize = 8 * 1024 * 1024 * 1024;
const MAX_RENAME_SCAN_FILE_BYTES: usize = 16 * 1024 * 1024;
const MAX_RENAME_INCLUDE_FILES: usize = 4_096;
const MAX_RENAME_INCLUDE_BYTES: usize = 256 * 1024 * 1024;
const MAX_RENAME_INCLUDE_DIRECTIVES: usize = 16_384;
const MAX_RENAME_INCLUDE_ERRORS: usize = 256;
const MAX_RENAME_INCLUDE_DEPTH: usize = 64;
const MAX_RENAME_INCLUDE_OWNER_SUMMARY_BYTES: usize = 64 * 1024;
const MAX_RENAME_INCLUDE_OWNER_DISCOVERY: usize = 256;
const MAX_SNAPSHOT_PHYSICAL_LOCATIONS: usize = 10_000;
const MAX_SNAPSHOT_MAPPING_WORK: usize = 1_000_000;
const MAX_RENAME_CONFIG_BYTES: usize = 4 * 1024 * 1024;
const MAX_AUTO_IMPORT_PROVIDER_SOURCES: usize = 512;
const INCLUDE_BYTE_BUDGET_ERROR: &str =
    "include byte limit would be exceeded before reading the file";

#[cfg(test)]
thread_local! {
    static TEST_CANCEL_INCLUDE_ANALYSIS: Cell<bool> = const { Cell::new(false) };
}

#[cfg(test)]
pub(crate) struct TestIncludeCancellationGuard(bool);

#[cfg(test)]
pub(crate) fn test_cancel_in_include_analysis() -> TestIncludeCancellationGuard {
    let previous = TEST_CANCEL_INCLUDE_ANALYSIS.with(|cancel| {
        let previous = cancel.get();
        cancel.set(true);
        previous
    });
    TestIncludeCancellationGuard(previous)
}

#[cfg(test)]
impl Drop for TestIncludeCancellationGuard {
    fn drop(&mut self) {
        TEST_CANCEL_INCLUDE_ANALYSIS.with(|cancel| cancel.set(self.0));
    }
}

#[derive(Debug, Clone)]
pub(crate) struct OverlayInput {
    pub(crate) text: String,
    pub(crate) version: i32,
}

#[derive(Debug, Clone)]
pub(crate) struct CachedDocument {
    pub(crate) context: ProjectContext,
    pub(crate) parsed: Arc<ParsedDocument>,
}

#[derive(Debug, Clone)]
pub(crate) struct WorkspaceInput {
    pub(crate) roots: Vec<PathBuf>,
    pub(crate) options: WorkspaceOptions,
    pub(crate) overrides: pascal_project::delphi_overrides::OverrideSession,
    pub(crate) project_selections: ProjectSelections,
    pub(crate) document_owners: HashMap<Url, KnownDocumentOwner>,
    pub(crate) overlays: HashMap<Url, OverlayInput>,
    pub(crate) cached_documents: HashMap<Url, CachedDocument>,
    pub(crate) rejected_documents: HashSet<Url>,
    pub(crate) rejection_reasons: HashMap<Url, String>,
    pub(crate) document_versions: HashMap<Url, i32>,
    pub(crate) deleted_overrides: HashMap<Url, Option<DiskStamp>>,
    pub(crate) source_generation: u64,
    pub(crate) configuration_generation: u64,
}

/// The small portion of a workspace snapshot needed to revalidate a retained
/// result after its worker has completed.  Full analysis inputs also retain
/// parsed documents, ownership maps, and project metadata; keeping those
/// structures alive for every backpressured partial delivery would defeat the
/// delivery memory bound.
#[derive(Debug, Clone)]
pub(crate) struct RevalidationInput {
    pub(crate) options: WorkspaceOptions,
    pub(crate) overlays: HashMap<Url, OverlayInput>,
}

impl RevalidationInput {
    pub(crate) fn retained_bytes(&self) -> usize {
        let option_bytes = self
            .options
            .source_paths
            .iter()
            .map(String::len)
            .chain(self.options.exclude.iter().map(String::len))
            .sum::<usize>()
            .saturating_add(
                self.options
                    .project_file
                    .as_ref()
                    .map_or(0, |path| path.to_string_lossy().len()),
            )
            .saturating_add(self.options.build_config.as_ref().map_or(0, String::len))
            .saturating_add(self.options.platform.as_ref().map_or(0, String::len));
        let overlay_bytes = self.overlays.iter().fold(0usize, |total, (uri, overlay)| {
            total
                .saturating_add(uri.as_str().len())
                .saturating_add(overlay.text.len())
                .saturating_add(std::mem::size_of::<OverlayInput>())
        });
        std::mem::size_of::<Self>()
            .saturating_add(std::mem::size_of::<WorkspaceOptions>())
            .saturating_add(option_bytes)
            .saturating_add(overlay_bytes)
    }
}

#[derive(Debug, Clone)]
pub(crate) struct SourceRecord {
    pub(crate) uri: Url,
    pub(crate) text: String,
    pub(crate) version: Option<i32>,
    pub(crate) stamp: Option<DiskStamp>,
    pub(crate) open: bool,
    pub(crate) path: Option<PathBuf>,
    pub(crate) path_stamp: Option<PathStamp>,
    pub(crate) content_hash: Option<u64>,
    /// Hash of the decoded source text that was actually parsed.  This is
    /// retained separately from `content_hash`: a full source record may have
    /// received a later raw-byte fingerprint, and that fingerprint must not
    /// replace equality with the parsed source.
    pub(crate) parsed_text_hash: Option<u64>,
    pub(crate) content_bytes: Option<Vec<u8>>,
    pub(crate) candidate_membership: Option<ProjectCandidateMembership>,
    /// Bounded resolver candidate observations.  These are semantic lookup
    /// inputs rather than directory-wide dependencies: a case-insensitive
    /// provider overlay can change resolution even when it has no disk record.
    pub(crate) candidate_observations: Vec<ResolverCandidateObservation>,
    /// The requester-scoped authorization used to read this closed source.
    /// Open overlays do not need these values because their payload is already
    /// supplied by the client and revalidation compares the overlay text.
    pub(crate) read_policy: Option<ReadPolicy>,
    pub(crate) path_entry: Option<ProjectPathEntry>,
    pub(crate) include_payload: bool,
    /// The recorded provider filename was absent during the computation.
    /// Unlike a positive source URI, this is invalidated by a matching source
    /// change even when the file was not part of the worker's read set.
    pub(crate) missing_provider_candidate: bool,
    /// The resolver observed the directory contents while resolving a source.
    /// A child create/delete/rename invalidates this record, but unrelated
    /// source records remain exact-path dependencies.
    pub(crate) directory_observation: bool,
    /// A complete projectless recursive filename lookup found no provider with
    /// any of these names below this root.  The scope is retained separately
    /// from the synthetic direct candidate so live validation can notice a
    /// provider overlay in an existing nested directory without rescanning.
    pub(crate) missing_provider_scope: Option<MissingProviderScope>,
    /// This path was read as part of bounded auto-import provider discovery.
    /// It distinguishes provider-source evidence from unrelated configuration
    /// and directory observations when an overlay supersedes the disk path.
    pub(crate) auto_import_provider_observation: bool,
    /// Bounded semantic observations used to prove that auto-import provider
    /// uniqueness remains fresh without invalidating on unrelated comments.
    pub(crate) auto_import_scopes: Vec<AutoImportProviderScope>,
}

pub(crate) fn source_record_owned_bytes(record: &SourceRecord) -> usize {
    std::mem::size_of::<SourceRecord>()
        .saturating_add(record.uri.as_str().len())
        .saturating_add(record.text.len())
        .saturating_add(
            record
                .path
                .as_ref()
                .map_or(0, |path| path.as_os_str().len()),
        )
        .saturating_add(record.content_bytes.as_ref().map_or(0, Vec::len))
        .saturating_add(
            record
                .candidate_observations
                .len()
                .saturating_mul(std::mem::size_of::<ResolverCandidateObservation>()),
        )
        .saturating_add(
            record
                .candidate_observations
                .iter()
                .map(|observation| observation.path.as_os_str().len())
                .sum::<usize>(),
        )
        .saturating_add(
            record
                .auto_import_scopes
                .len()
                .saturating_mul(std::mem::size_of::<AutoImportProviderScope>()),
        )
        .saturating_add(
            record
                .auto_import_scopes
                .iter()
                .map(|scope| {
                    scope.root.as_os_str().len()
                        + scope.provider_units.iter().map(String::len).sum::<usize>()
                        + scope
                            .candidate_prefixes
                            .iter()
                            .map(String::len)
                            .sum::<usize>()
                })
                .sum::<usize>(),
        )
        .saturating_add(record.missing_provider_scope.as_ref().map_or(0, |scope| {
            scope.root.as_os_str().len() + scope.names.iter().map(String::len).sum::<usize>()
        }))
}

pub(crate) const MAX_RESOLVER_CANDIDATE_OBSERVATIONS: usize = 4_096;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResolverCandidateObservation {
    pub(crate) path: PathBuf,
    pub(crate) present: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MissingProviderScope {
    pub(crate) root: PathBuf,
    pub(crate) names: Vec<String>,
    pub(crate) read_policy: ReadPolicy,
    pub(crate) path_entry: ProjectPathEntry,
}

impl MissingProviderScope {
    pub(crate) fn matches(&self, path: &Path) -> bool {
        path_starts_with_ci(path, &self.root)
            && path.file_name().is_some_and(|file_name| {
                self.names.iter().any(|name| {
                    file_name
                        .to_string_lossy()
                        .eq_ignore_ascii_case(name.as_str())
                })
            })
    }

    pub(crate) fn allows_without_filesystem(&self, path: &Path) -> bool {
        self.read_policy
            .allows_path_without_filesystem(path, &self.path_entry.provenance)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AutoImportProviderScope {
    pub(crate) root: PathBuf,
    pub(crate) provider_units: Vec<String>,
    pub(crate) candidate_prefixes: Vec<String>,
    pub(crate) read_policy: ReadPolicy,
    pub(crate) path_entry: ProjectPathEntry,
}

impl AutoImportProviderScope {
    pub(crate) fn matches_path(&self, path: &Path) -> bool {
        path_starts_with_ci(path, &self.root)
            && self
                .read_policy
                .allows_path_without_filesystem(path, &self.path_entry.provenance)
    }

    pub(crate) fn path_is_accepted(&self, workspace: &Workspace, path: &Path) -> bool {
        workspace.scope_path_is_accepted_for_auto_import(path, self)
    }
}

pub(crate) fn auto_import_source_is_relevant(
    source: &str,
    scope: &AutoImportProviderScope,
) -> bool {
    let cancel = AtomicBool::new(false);
    match source_unit_name(source, &cancel) {
        Ok(Some(unit_name))
            if scope
                .provider_units
                .iter()
                .any(|provider| provider.eq_ignore_ascii_case(&unit_name)) =>
        {
            true
        }
        Err(_) => true,
        _ => contains_any_identifier_prefix(source, &scope.candidate_prefixes),
    }
}

impl SourceRecord {
    fn payload_dependency(&self) -> Result<(&ReadPolicy, &ProjectPathEntry), String> {
        match (self.read_policy.as_ref(), self.path_entry.as_ref()) {
            (Some(read_policy), Some(path_entry)) => Ok((read_policy, path_entry)),
            _ => Err("closed source has no requester-scoped read authorization".to_string()),
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct SnapshotSeed {
    pub(crate) record: SourceRecord,
    pub(crate) consumed_configuration: Vec<SourceRecord>,
    pub(crate) completion_position: Option<Position>,
}

impl SnapshotSeed {
    pub(crate) fn new(record: SourceRecord) -> Self {
        Self {
            record,
            consumed_configuration: Vec::new(),
            completion_position: None,
        }
    }

    pub(crate) fn with_consumed_configuration(mut self, records: &[SourceRecord]) -> Self {
        self.consumed_configuration = records.to_vec();
        self
    }

    pub(crate) fn with_completion_position(mut self, position: Option<Position>) -> Self {
        self.completion_position = position;
        self
    }
}

#[derive(Debug)]
pub(crate) struct Computed<T> {
    pub(crate) source_generation: u64,
    pub(crate) configuration_generation: u64,
    pub(crate) value: Result<T, String>,
    pub(crate) records: Vec<SourceRecord>,
}

pub(crate) struct RenameSnapshot {
    pub(crate) index: NavigationIndex,
    pub(crate) sources: HashMap<Url, String>,
    pub(crate) records: HashMap<Url, SourceRecord>,
    pub(crate) expansions: HashMap<Url, super::ExpansionRecord>,
    pub(crate) readable: HashSet<Url>,
    pub(crate) editable: HashSet<Url>,
    pub(crate) complete: bool,
    pub(crate) incomplete_reason: Option<String>,
    pub(crate) include_errors: Vec<String>,
    pub(crate) baseline_records: Vec<SourceRecord>,
    pub(crate) mode: SnapshotMode,
}

fn cached_position_index<'a>(
    cache: &'a mut HashMap<Url, text::PositionIndex>,
    uri: &Url,
    source: &str,
    budget: &include_expansion::MappingBudget<'_>,
) -> Result<&'a text::PositionIndex, String> {
    if !cache.contains_key(uri) {
        let index = text::PositionIndex::new_with_cancel(source, budget.cancellation())
            .map_err(|()| CANCELLATION_MESSAGE.to_string())?;
        cache.insert(uri.clone(), index);
    }
    Ok(cache.get(uri).expect("position index was inserted"))
}

impl RenameSnapshot {
    fn virtual_query_positions_with_budget(
        &self,
        uri: &Url,
        position: Position,
        budget: &mut include_expansion::MappingBudget<'_>,
    ) -> Result<Vec<(Url, Position)>, String> {
        let Some(source) = self
            .sources
            .get(uri)
            .or_else(|| self.records.get(uri).map(|record| &record.text))
        else {
            return Ok(vec![(uri.clone(), position)]);
        };
        let Some(offset) = text::position_to_offset(source, position) else {
            return Ok(Vec::new());
        };
        if offset >= source.len() {
            return Ok(vec![(uri.clone(), position)]);
        }
        let width = source[offset..].chars().next().map_or(1, char::len_utf8);
        let physical_range = offset..offset.saturating_add(width);
        let mut positions = Vec::new();
        let mut mapped_by_expansion = false;
        for (root_uri, expansion) in &self.expansions {
            let virtual_ranges = expansion.expanded.reverse_range_with_budget(
                uri,
                physical_range.clone(),
                budget,
            )?;
            if !virtual_ranges.is_empty() {
                mapped_by_expansion = true;
            }
            if !expansion.complete {
                continue;
            }
            for virtual_range in virtual_ranges {
                if let Some(virtual_position) =
                    text::offset_to_position(expansion.expanded.text(), virtual_range.start)
                {
                    positions.push((root_uri.clone(), virtual_position));
                }
            }
        }
        if positions.is_empty() && !mapped_by_expansion && self.index.contains(uri) {
            positions.push((uri.clone(), position));
        }
        positions.sort_by(|left, right| {
            left.0
                .as_str()
                .cmp(right.0.as_str())
                .then_with(|| left.1.line.cmp(&right.1.line))
                .then_with(|| left.1.character.cmp(&right.1.character))
        });
        positions.dedup();
        Ok(positions)
    }

    fn map_location_with_budget(
        &self,
        location: Location,
        budget: &mut include_expansion::MappingBudget<'_>,
        virtual_indexes: &mut HashMap<Url, text::PositionIndex>,
        physical_indexes: &mut HashMap<Url, text::PositionIndex>,
    ) -> Result<Vec<Location>, String> {
        let Some(expansion) = self.expansions.get(&location.uri) else {
            return Ok(vec![location]);
        };
        if !expansion.complete {
            return Ok(Vec::new());
        }
        let virtual_index = cached_position_index(
            virtual_indexes,
            &location.uri,
            expansion.expanded.text(),
            budget,
        )?;
        let Some(start) =
            virtual_index.position_to_offset(expansion.expanded.text(), location.range.start)
        else {
            return Ok(Vec::new());
        };
        let Some(end) =
            virtual_index.position_to_offset(expansion.expanded.text(), location.range.end)
        else {
            return Ok(Vec::new());
        };
        let spans = match expansion
            .expanded
            .map_range_with_budget(start..end, budget)?
        {
            crate::include_expansion::VirtualMapping::Exact(span) => vec![span],
            crate::include_expansion::VirtualMapping::Many(spans) => spans,
            crate::include_expansion::VirtualMapping::Unmapped => return Ok(Vec::new()),
        };
        let mut mapped = Vec::new();
        for span in spans {
            let Some(source) = expansion
                .source_texts
                .get(&span.uri)
                .or_else(|| self.sources.get(&span.uri))
            else {
                continue;
            };
            let physical_index =
                cached_position_index(physical_indexes, &span.uri, source, budget)?;
            let Some(start) = physical_index.offset_to_position(source, span.range.start) else {
                continue;
            };
            let Some(end) = physical_index.offset_to_position(source, span.range.end) else {
                continue;
            };
            mapped.push(Location::new(span.uri, Range::new(start, end)));
        }
        Ok(mapped)
    }

    fn physical_span_is_contextually_repeated(
        &self,
        uri: &Url,
        range: &std::ops::Range<usize>,
        budget: &mut include_expansion::MappingBudget<'_>,
    ) -> Result<bool, String> {
        let mut owners = 0usize;
        for expansion in self
            .expansions
            .values()
            .filter(|expansion| expansion.complete)
        {
            let occurrences =
                expansion
                    .expanded
                    .reverse_range_with_budget(uri, range.clone(), budget)?;
            if occurrences.len() > 1 {
                return Ok(true);
            }
            if !occurrences.is_empty() {
                owners = owners.saturating_add(1);
                if owners > 1 {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    fn prepare_rename(
        &self,
        uri: &Url,
        position: Position,
        cancel: &AtomicBool,
    ) -> Result<PrepareRenameResponse, String> {
        let mut budget = include_expansion::MappingBudget::new(cancel, MAX_SNAPSHOT_MAPPING_WORK);
        let mut virtual_indexes = HashMap::new();
        let mut physical_indexes = HashMap::new();
        let mut ranges = Vec::new();
        for (query_uri, query_position) in
            self.virtual_query_positions_with_budget(uri, position, &mut budget)?
        {
            let response = self.index.prepare_rename(&query_uri, query_position)?;
            let PrepareRenameResponse::Range(range) = response else {
                return Err("rename target has unsupported placeholder metadata".to_string());
            };
            let locations = self.map_location_with_budget(
                Location::new(query_uri, range),
                &mut budget,
                &mut virtual_indexes,
                &mut physical_indexes,
            )?;
            if locations.len() != 1 {
                return Err("rename target does not map to one physical source range".to_string());
            }
            ranges.push(locations[0].range);
        }
        ranges.sort_by_key(|range| {
            (
                range.start.line,
                range.start.character,
                range.end.line,
                range.end.character,
            )
        });
        ranges.dedup();
        ranges
            .into_iter()
            .next()
            .map(PrepareRenameResponse::Range)
            .ok_or_else(|| "rename target is unresolved or ambiguous".to_string())
    }

    fn rename_edits(
        &self,
        uri: &Url,
        position: Position,
        new_name: &str,
        cancel: &AtomicBool,
    ) -> Result<HashMap<Url, Vec<TextEdit>>, String> {
        let mut budget = include_expansion::MappingBudget::new(cancel, MAX_SNAPSHOT_MAPPING_WORK);
        let mut virtual_indexes = HashMap::new();
        let mut physical_indexes = HashMap::new();
        let mut edits = HashMap::new();
        let query_positions =
            self.virtual_query_positions_with_budget(uri, position, &mut budget)?;
        for (query_uri, query_position) in query_positions {
            let raw = self
                .index
                .rename_edits(&query_uri, query_position, new_name)?;
            for (edit_uri, document_edits) in raw {
                for edit in document_edits {
                    let Some(expansion) = self.expansions.get(&edit_uri) else {
                        edits
                            .entry(edit_uri.clone())
                            .or_insert_with(Vec::new)
                            .push(edit);
                        continue;
                    };
                    if !expansion.complete {
                        return Err("rename include expansion is incomplete".to_string());
                    }
                    let virtual_index = cached_position_index(
                        &mut virtual_indexes,
                        &edit_uri,
                        expansion.expanded.text(),
                        &budget,
                    )?;
                    let Some(start) = virtual_index
                        .position_to_offset(expansion.expanded.text(), edit.range.start)
                    else {
                        return Err("rename edit starts outside the expanded source".to_string());
                    };
                    let Some(end) =
                        virtual_index.position_to_offset(expansion.expanded.text(), edit.range.end)
                    else {
                        return Err("rename edit ends outside the expanded source".to_string());
                    };
                    let span = match expansion
                        .expanded
                        .map_range_with_budget(start..end, &mut budget)?
                    {
                        crate::include_expansion::VirtualMapping::Exact(span) => span,
                        crate::include_expansion::VirtualMapping::Many(_) => {
                            return Err("rename edit crosses physical include segments".to_string());
                        }
                        crate::include_expansion::VirtualMapping::Unmapped => {
                            return Err("rename edit maps to synthetic include text".to_string());
                        }
                    };
                    if self.physical_span_is_contextually_repeated(
                        &span.uri,
                        &span.range,
                        &mut budget,
                    )? {
                        return Err(
                            "rename edit has multiple contextual physical owners; refusing an ambiguous include edit"
                                .to_string(),
                        );
                    }
                    let source = expansion
                        .source_texts
                        .get(&span.uri)
                        .or_else(|| self.sources.get(&span.uri))
                        .ok_or_else(|| format!("rename source was not retained: {}", span.uri))?;
                    let physical_index =
                        cached_position_index(&mut physical_indexes, &span.uri, source, &budget)?;
                    let start = physical_index
                        .offset_to_position(source, span.range.start)
                        .ok_or_else(|| "rename edit has an invalid physical start".to_string())?;
                    let end = physical_index
                        .offset_to_position(source, span.range.end)
                        .ok_or_else(|| "rename edit has an invalid physical end".to_string())?;
                    let mapped = TextEdit::new(Range::new(start, end), edit.new_text);
                    let target = edits.entry(span.uri).or_insert_with(Vec::new);
                    if let Some(existing) = target
                        .iter()
                        .find(|existing| existing.range == mapped.range)
                    {
                        if existing.new_text != mapped.new_text {
                            return Err("rename produced conflicting physical edits".to_string());
                        }
                    } else {
                        target.push(mapped);
                    }
                }
            }
        }
        for document_edits in edits.values_mut() {
            document_edits.sort_by_key(|edit| {
                (
                    edit.range.start.line,
                    edit.range.start.character,
                    edit.range.end.line,
                    edit.range.end.character,
                )
            });
        }
        Ok(edits)
    }

    pub(crate) fn binding_locations(
        &self,
        uri: &Url,
        position: Position,
        include_declaration: bool,
        cancel: &AtomicBool,
    ) -> Result<Vec<Location>, String> {
        let mut locations = Vec::new();
        let mut seen = HashSet::new();
        let mut budget = include_expansion::MappingBudget::new(cancel, MAX_SNAPSHOT_MAPPING_WORK);
        let mut virtual_indexes = HashMap::new();
        let mut physical_indexes = HashMap::new();
        let mut resolution_budget =
            crate::navigation::BindingWorkBudget::new(MAX_SNAPSHOT_MAPPING_WORK);
        let query_positions =
            self.virtual_query_positions_with_budget(uri, position, &mut budget)?;
        for (query_uri, query_position) in query_positions {
            resolution_budget.charge()?;
            let query_locations = self.index.binding_locations_with_cancel_and_work_budget(
                &query_uri,
                query_position,
                include_declaration,
                cancel,
                &mut resolution_budget,
            )?;
            for location in query_locations {
                for mapped in self.map_location_with_budget(
                    location,
                    &mut budget,
                    &mut virtual_indexes,
                    &mut physical_indexes,
                )? {
                    if is_cancelled(cancel) {
                        return Err(CANCELLATION_MESSAGE.to_string());
                    }
                    let key = (
                        mapped.uri.as_str().to_owned(),
                        mapped.range.start.line,
                        mapped.range.start.character,
                        mapped.range.end.line,
                        mapped.range.end.character,
                    );
                    if seen.insert(key) {
                        if locations.len() >= MAX_SNAPSHOT_PHYSICAL_LOCATIONS {
                            return Err(format!(
                                "binding reference result exceeds the {MAX_SNAPSHOT_PHYSICAL_LOCATIONS}-entry limit"
                            ));
                        }
                        locations.push(mapped);
                    }
                }
            }
        }
        locations.sort_by(|left, right| {
            left.uri
                .as_str()
                .cmp(right.uri.as_str())
                .then_with(|| left.range.start.line.cmp(&right.range.start.line))
                .then_with(|| left.range.start.character.cmp(&right.range.start.character))
        });
        Ok(locations)
    }

    pub(crate) fn binding_highlights_in_document(
        &self,
        uri: &Url,
        position: Position,
        cancel: &AtomicBool,
    ) -> Result<Vec<DocumentHighlight>, String> {
        let mut highlights = Vec::new();
        let mut seen = HashSet::new();
        let mut budget = include_expansion::MappingBudget::new(cancel, MAX_SNAPSHOT_MAPPING_WORK);
        let mut virtual_indexes = HashMap::new();
        let mut physical_indexes = HashMap::new();
        let mut resolution_budget =
            crate::navigation::BindingWorkBudget::new(MAX_SNAPSHOT_MAPPING_WORK);
        let query_positions =
            self.virtual_query_positions_with_budget(uri, position, &mut budget)?;
        for (query_uri, query_position) in query_positions {
            resolution_budget.charge()?;
            let query_highlights = self
                .index
                .binding_highlights_in_document_with_cancel_and_work_budget(
                    &query_uri,
                    query_position,
                    cancel,
                    &mut resolution_budget,
                )?;
            for highlight in query_highlights {
                for mapped in self.map_location_with_budget(
                    Location::new(query_uri.clone(), highlight.range),
                    &mut budget,
                    &mut virtual_indexes,
                    &mut physical_indexes,
                )? {
                    if is_cancelled(cancel) {
                        return Err(CANCELLATION_MESSAGE.to_string());
                    }
                    if mapped.uri != *uri {
                        continue;
                    }
                    let key = (
                        mapped.range.start.line,
                        mapped.range.start.character,
                        mapped.range.end.line,
                        mapped.range.end.character,
                    );
                    if seen.insert(key) {
                        if highlights.len() >= MAX_SNAPSHOT_PHYSICAL_LOCATIONS {
                            return Err(format!(
                                "binding reference result exceeds the {MAX_SNAPSHOT_PHYSICAL_LOCATIONS}-entry limit"
                            ));
                        }
                        highlights.push(DocumentHighlight {
                            range: mapped.range,
                            kind: highlight.kind,
                        });
                    }
                }
            }
        }
        highlights.sort_by_key(|highlight| {
            (
                highlight.range.start.line,
                highlight.range.start.character,
                highlight.range.end.line,
                highlight.range.end.character,
            )
        });
        Ok(highlights)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SnapshotMode {
    /// Retain only the requested source and its directly required parse state.
    /// This is sufficient for routine-local bindings, which cannot be
    /// referenced by another unit.
    Local,
    /// Retain the requested source and its bounded import closure, without
    /// enumerating reverse workspace consumers. This is used by document
    /// highlights for imported bindings.
    LocalWithImports,
    /// Retain the requested source and its bounded import closure for
    /// name-free completion and signature help. Includes are audited for
    /// declaration-bearing content because those features cannot filter an
    /// include audit by a selected binding name.
    Assistance,
    /// Search the configured workspace for reverse references.
    Workspace,
    /// Search the configured workspace for read-only symbol results.
    WorkspaceSymbols,
}

fn context_incomplete_for_mode(mode: SnapshotMode, context: &ProjectContext) -> bool {
    match mode {
        SnapshotMode::Workspace => !context.discovery_complete,
        SnapshotMode::WorkspaceSymbols => context.override_error.is_some(),
        SnapshotMode::Local | SnapshotMode::LocalWithImports | SnapshotMode::Assistance => false,
    }
}

#[cfg(test)]
type SnapshotPriorityBarrier = (Sender<()>, Receiver<()>);

#[cfg(test)]
static SNAPSHOT_PRIORITY_BARRIERS: OnceLock<Mutex<HashMap<Url, SnapshotPriorityBarrier>>> =
    OnceLock::new();

#[cfg(test)]
pub(crate) fn install_snapshot_priority_barrier(
    priority_uri: Url,
    ready: Sender<()>,
    release: Receiver<()>,
) {
    SNAPSHOT_PRIORITY_BARRIERS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .expect("snapshot barrier lock")
        .insert(priority_uri, (ready, release));
}

#[cfg(test)]
fn wait_at_snapshot_priority_barrier(priority: &[Url]) {
    let Some(priority_uri) = priority.first() else {
        return;
    };
    let barrier = SNAPSHOT_PRIORITY_BARRIERS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .expect("snapshot barrier lock")
        .remove(priority_uri);
    if let Some((ready, release)) = barrier {
        ready.send(()).expect("snapshot barrier ready receiver");
        release.recv().expect("snapshot barrier release sender");
    }
}

#[derive(Debug, Default)]
struct Enumeration {
    paths: Vec<EnumeratedSource>,
    path_indices: HashMap<String, usize>,
    contexts: HashMap<ContextKey, ContextState>,
    #[cfg(test)]
    path_lookups: usize,
    baseline: BaselineAccumulator,
    baseline_content_hashes: HashMap<String, u64>,
    baseline_contents: HashMap<String, Vec<u8>>,
    complete: bool,
    reason: Option<String>,
    visited_entries: usize,
    auto_import_complete: bool,
    auto_import_unit_providers: HashMap<String, Vec<Url>>,
}

#[derive(Debug, Clone)]
struct EnumeratedSource {
    path: PathBuf,
    owner: Option<ContextKey>,
}

impl Enumeration {
    fn add_path(&mut self, path: PathBuf, owner: Option<ContextKey>) {
        let key = path_key(&path);
        if let Some(index) = self.lookup_index(&key) {
            let existing = &mut self.paths[index];
            if let Some(owner) = owner {
                if let Some(existing_owner) = &existing.owner {
                    if existing_owner != &owner {
                        self.complete = false;
                        self.reason.get_or_insert_with(|| {
                            format!(
                                "source {path:?} was discovered under incompatible project contexts"
                            )
                        });
                    }
                } else {
                    existing.owner = Some(owner);
                }
            }
            return;
        }
        let index = self.paths.len();
        self.path_indices.insert(key, index);
        self.paths.push(EnumeratedSource { path, owner });
    }

    fn assign_owner(&mut self, path: &Path, owner: ContextKey) {
        let key = path_key(path);
        let Some(index) = self.lookup_index(&key) else {
            return;
        };
        let existing = &mut self.paths[index];
        if let Some(existing_owner) = &existing.owner {
            if existing_owner != &owner {
                self.complete = false;
                self.reason.get_or_insert_with(|| {
                    format!("source {path:?} was discovered under incompatible project contexts")
                });
            }
        } else {
            existing.owner = Some(owner);
        }
    }

    fn retain_context(&mut self, key: ContextKey, state: ContextState) {
        match self.contexts.entry(key) {
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(state);
            }
            std::collections::hash_map::Entry::Occupied(entry) => {
                if !evaluated_contexts_equal(&entry.get().context, &state.context) {
                    self.complete = false;
                    self.reason.get_or_insert_with(|| {
                        "project context changed while building the workspace snapshot".to_string()
                    });
                }
            }
        }
    }

    fn lookup_index(&mut self, key: &str) -> Option<usize> {
        #[cfg(test)]
        {
            self.path_lookups = self.path_lookups.saturating_add(1);
        }
        self.path_indices.get(key).copied()
    }

    #[cfg(test)]
    fn lookup_count(&self) -> usize {
        self.path_lookups
    }

    fn sort_paths(&mut self, priority: &[Url]) {
        self.paths.sort_by(|left, right| {
            left.path
                .to_string_lossy()
                .cmp(&right.path.to_string_lossy())
        });
        let priority_paths = priority
            .iter()
            .filter_map(|uri| uri.to_file_path().ok().map(absolute_path))
            .collect::<Vec<_>>();
        self.paths.sort_by_key(|source| {
            priority_paths
                .iter()
                .position(|priority| paths_equal_ci(priority, &source.path))
                .unwrap_or(priority_paths.len())
        });
        self.path_indices.clear();
        for (index, source) in self.paths.iter().enumerate() {
            self.path_indices.insert(path_key(&source.path), index);
        }
    }
}

fn evaluated_contexts_equal(left: &ProjectContext, right: &ProjectContext) -> bool {
    let mut left = left.clone();
    let mut right = right.clone();
    // These fields are observations accumulated while reading, not evaluated
    // project state. They may legitimately grow as a snapshot discovers more
    // metadata, while changed search paths/overrides must invalidate it.
    left.metadata_files.clear();
    left.metadata_observations.clear();
    left.warnings.clear();
    right.metadata_files.clear();
    right.metadata_observations.clear();
    right.warnings.clear();
    normalize_standalone_legacy_roots(&mut left);
    normalize_standalone_legacy_roots(&mut right);
    left == right
}

fn normalize_standalone_legacy_roots(context: &mut ProjectContext) {
    if context.project_file.is_some() {
        return;
    }
    let legacy_roots = context
        .search_path_entries
        .iter()
        .filter(|entry| matches!(entry.provenance, ProjectPathProvenance::LegacyNative))
        .map(|entry| entry.path.clone())
        .collect::<Vec<_>>();
    context
        .search_path_entries
        .retain(|entry| !matches!(entry.provenance, ProjectPathProvenance::LegacyNative));
    context.search_paths.retain(|path| {
        !legacy_roots
            .iter()
            .any(|legacy_root| paths_equal_ci(path, legacy_root))
    });
}

#[derive(Debug, Clone)]
struct BaselinePath {
    path: PathBuf,
    stamp: Option<PathStamp>,
    candidate_membership: Option<ProjectCandidateMembership>,
    read_policy: Option<ReadPolicy>,
    path_entry: Option<ProjectPathEntry>,
    include_payload: bool,
}

#[derive(Debug, Default)]
struct BaselineAccumulator {
    paths: Vec<BaselinePath>,
    indices: HashMap<String, usize>,
    #[cfg(test)]
    key_lookups: usize,
}

impl BaselineAccumulator {
    fn add_path(&mut self, path: PathBuf, stamp: Option<PathStamp>) {
        let key = path_key(&path);
        if let Some(index) = self.lookup_index(&key) {
            let existing = &mut self.paths[index];
            if existing.stamp.is_none() && existing.read_policy.is_none() {
                existing.stamp = stamp;
            }
            return;
        }
        let index = self.paths.len();
        self.indices.insert(key, index);
        self.paths.push(BaselinePath {
            path,
            stamp,
            candidate_membership: None,
            read_policy: None,
            path_entry: None,
            include_payload: false,
        });
    }

    fn add_candidate_membership(
        &mut self,
        path: PathBuf,
        membership: ProjectCandidateMembership,
        observe_directory_stamp: bool,
    ) {
        let key = path_key(&path);
        if let Some(index) = self.lookup_index(&key) {
            self.paths[index].candidate_membership = Some(membership);
            return;
        }
        let index = self.paths.len();
        self.indices.insert(key, index);
        let stamp = observe_directory_stamp.then(|| path_stamp(&path)).flatten();
        self.paths.push(BaselinePath {
            path,
            stamp,
            candidate_membership: Some(membership),
            read_policy: None,
            path_entry: None,
            include_payload: false,
        });
    }

    fn set_payload_dependency(
        &mut self,
        path: &Path,
        read_policy: ReadPolicy,
        path_entry: ProjectPathEntry,
    ) {
        let key = path_key(path);
        let index = self.lookup_index(&key).unwrap_or_else(|| {
            let index = self.paths.len();
            self.indices.insert(key, index);
            self.paths.push(BaselinePath {
                path: path.to_path_buf(),
                stamp: path_stamp(path),
                candidate_membership: None,
                read_policy: None,
                path_entry: None,
                include_payload: false,
            });
            index
        });
        if self.paths[index].read_policy.is_some() {
            return;
        }
        self.paths[index].read_policy = Some(read_policy);
        self.paths[index].path_entry = Some(path_entry);
        self.paths[index].include_payload = false;
    }

    fn set_payload_observation(
        &mut self,
        path: &Path,
        stamp: Option<PathStamp>,
        read_policy: ReadPolicy,
        path_entry: ProjectPathEntry,
    ) {
        let key = path_key(path);
        let index = self.lookup_index(&key).unwrap_or_else(|| {
            let index = self.paths.len();
            self.indices.insert(key, index);
            self.paths.push(BaselinePath {
                path: path.to_path_buf(),
                stamp: None,
                candidate_membership: None,
                read_policy: None,
                path_entry: None,
                include_payload: false,
            });
            index
        });
        if self.paths[index].read_policy.is_some() {
            return;
        }
        self.paths[index].stamp = stamp;
        self.paths[index].read_policy = Some(read_policy);
        self.paths[index].path_entry = Some(path_entry);
        self.paths[index].include_payload = false;
    }

    fn set_include_payload_dependency(
        &mut self,
        path: &Path,
        read_policy: ReadPolicy,
        path_entry: ProjectPathEntry,
    ) {
        let key = path_key(path);
        let index = self.lookup_index(&key).unwrap_or_else(|| {
            let index = self.paths.len();
            self.indices.insert(key, index);
            self.paths.push(BaselinePath {
                path: path.to_path_buf(),
                stamp: path_stamp(path),
                candidate_membership: None,
                read_policy: None,
                path_entry: None,
                include_payload: false,
            });
            index
        });
        if self.paths[index].read_policy.is_some() {
            return;
        }
        self.paths[index].read_policy = Some(read_policy);
        self.paths[index].path_entry = Some(path_entry);
        self.paths[index].include_payload = true;
    }

    fn lookup_index(&mut self, key: &str) -> Option<usize> {
        #[cfg(test)]
        {
            self.key_lookups = self.key_lookups.saturating_add(1);
        }
        self.indices.get(key).copied()
    }

    #[cfg(test)]
    fn lookup_count(&self) -> usize {
        self.key_lookups
    }
}

impl Workspace {
    pub(crate) fn revalidation_input(&self) -> RevalidationInput {
        let overlays = self
            .open_documents
            .iter()
            .filter_map(|(uri, document)| {
                let text = document.text.as_ref()?.clone();
                Some((
                    canonical_file_uri(uri),
                    OverlayInput {
                        text,
                        version: document.version,
                    },
                ))
            })
            .collect();
        RevalidationInput {
            options: self.options.clone(),
            overlays,
        }
    }

    pub(crate) fn analysis_input(&self) -> WorkspaceInput {
        let overlays = self
            .open_documents
            .iter()
            .filter_map(|(uri, document)| {
                let text = document.text.as_ref()?.clone();
                Some((
                    canonical_file_uri(uri),
                    OverlayInput {
                        text,
                        version: document.version,
                    },
                ))
            })
            .collect();
        let rejected_documents = self
            .open_documents
            .iter()
            .filter_map(|(uri, document)| {
                document.rejection.as_ref().map(|_| canonical_file_uri(uri))
            })
            .collect();
        let rejection_reasons = self
            .open_documents
            .iter()
            .filter_map(|(uri, document)| {
                document
                    .rejection
                    .as_ref()
                    .map(|reason| (canonical_file_uri(uri), reason.clone()))
            })
            .collect();
        let document_versions = self
            .open_documents
            .iter()
            .map(|(uri, document)| (canonical_file_uri(uri), document.version))
            .collect();
        let cached_documents = self
            .index
            .reusable_documents()
            .into_iter()
            .filter_map(|(uri, parsed)| {
                let context_key = self.document_contexts.get(&uri)?;
                let context = self.contexts.get(context_key)?.context.clone();
                Some((canonical_file_uri(&uri), CachedDocument { context, parsed }))
            })
            .collect();
        WorkspaceInput {
            roots: self.roots.iter().map(|root| root.path.clone()).collect(),
            options: self.options.clone(),
            overrides: self.overrides.clone(),
            project_selections: self.project_selections.clone(),
            document_owners: self.document_owners.clone(),
            overlays,
            cached_documents,
            rejected_documents,
            rejection_reasons,
            document_versions,
            deleted_overrides: self.deleted_overrides.clone(),
            source_generation: self.source_generation,
            configuration_generation: self.configuration_generation,
        }
    }

    /// Build and validate a complete snapshot synchronously for embedders that
    /// do not use the protocol worker. The server uses the `*_from_input`
    /// functions below so this work never runs on its protocol loop.
    pub fn prepare_rename(
        &mut self,
        uri: &Url,
        position: Position,
    ) -> Result<PrepareRenameResponse, String> {
        let cancel = AtomicBool::new(false);
        let computed = prepare_from_input(self.analysis_input(), uri, position, &cancel);
        self.finish_computation(computed)
    }

    pub fn rename_edits(
        &mut self,
        uri: &Url,
        position: Position,
        new_name: &str,
        document_changes: bool,
    ) -> Result<WorkspaceEdit, String> {
        let cancel = AtomicBool::new(false);
        let computed = rename_from_input(
            self.analysis_input(),
            uri,
            position,
            new_name,
            document_changes,
            &cancel,
        );
        self.finish_computation(computed)
    }

    fn finish_computation<T>(&self, computed: Computed<T>) -> Result<T, String> {
        if computed.source_generation != self.source_generation
            || computed.configuration_generation != self.configuration_generation
        {
            return Err("rename analysis became stale; retry the request".to_string());
        }
        self.revalidate_records(&computed.records)?;
        computed.value
    }

    pub(crate) fn revalidate_records(&self, records: &[SourceRecord]) -> Result<(), String> {
        let cancel = AtomicBool::new(false);
        for record in records {
            if let Some(path) = &record.path {
                if record.missing_provider_candidate {
                    let uri = Url::from_file_path(absolute_path(path.clone()))
                        .ok()
                        .map(|uri| canonical_file_uri(&uri));
                    if uri
                        .as_ref()
                        .and_then(|uri| self.open_documents.get(uri))
                        .is_some_and(|document| document.text.is_some())
                    {
                        return Err(format!(
                            "include provider appeared as an overlay while resolving {}; retry the request",
                            path.display()
                        ));
                    }
                    if record.path_stamp.is_none() && path_stamp(path).is_some() {
                        return Err(format!(
                            "include provider appeared on disk while resolving {}; retry the request",
                            path.display()
                        ));
                    }
                }
                revalidate_path_record(path, record, &cancel, true)?;
                continue;
            }
            if record.open {
                let Some(document) = self.open_documents.get(&record.uri) else {
                    return Err(format!(
                        "open document disappeared while resolving {}",
                        record.uri
                    ));
                };
                let text_changed = record.content_hash.map_or_else(
                    || document.text.as_deref() != Some(record.text.as_str()),
                    |expected| {
                        document.text.as_deref().is_none_or(|text| {
                            if record.text.is_empty() {
                                super::content_hash_bytes(text.as_bytes()) != expected
                            } else {
                                text_content_hash(text) != expected
                            }
                        })
                    },
                ) || document
                    .text
                    .as_deref()
                    .is_some_and(|text| parsed_source_changed(record, text));
                if document.version != record.version.unwrap_or_default() || text_changed {
                    return Err(format!(
                        "source changed while resolving {}; retry the request",
                        record.uri
                    ));
                }
                continue;
            }

            let path = record
                .uri
                .to_file_path()
                .map(absolute_path)
                .map_err(|_| format!("not a file URI: {}", record.uri))?;
            if disk_stamp(&path) != record.stamp {
                return Err(format!(
                    "closed source changed while resolving {}; retry the request",
                    record.uri
                ));
            }
            let (read_policy, path_entry) = record.payload_dependency().map_err(|error| {
                format!(
                    "closed source changed while resolving {}: {error}",
                    record.uri
                )
            })?;
            let allow_legacy_payload =
                matches!(&path_entry.provenance, ProjectPathProvenance::LegacyNative);
            let current = read_disk_source(
                &path,
                self.options.limits.max_file_bytes,
                read_policy,
                path_entry,
                allow_legacy_payload,
            )
            .map_err(|error| {
                format!(
                    "closed source changed while resolving {}: {error}",
                    record.uri
                )
            })?;
            let content_changed = record.content_hash.map_or_else(
                || current.text != record.text,
                |expected| expected != current.content_hash,
            ) || parsed_source_changed(record, &current.text);
            if content_changed {
                return Err(format!(
                    "closed source changed while resolving {}; retry the request",
                    record.uri
                ));
            }
        }
        Ok(())
    }
}

pub(crate) fn revalidate_input(
    input: &WorkspaceInput,
    records: &[SourceRecord],
    cancel: &AtomicBool,
) -> Result<(), String> {
    revalidate_records(&input.options, &input.overlays, records, cancel, true)
}

/// Revalidate the effective source/context identity of retained diagnostics.
///
/// Protocol versions and filesystem stamps are observations that can change
/// even when the bytes and effective source remain identical.  The ordinary
/// validation path intentionally rejects those observations for operations
/// whose edits depend on the exact captured transport state; related
/// diagnostic ownership instead needs the effective identity so a no-op
/// overlay or disk rewrite cannot clear another owner's still-current report.
pub(crate) fn revalidate_effective_input(
    input: &WorkspaceInput,
    records: &[SourceRecord],
    cancel: &AtomicBool,
) -> Result<(), String> {
    revalidate_records(&input.options, &input.overlays, records, cancel, false)
}

pub(crate) fn revalidate_revalidation_input(
    input: &RevalidationInput,
    records: &[SourceRecord],
    cancel: &AtomicBool,
) -> Result<(), String> {
    revalidate_records(&input.options, &input.overlays, records, cancel, true)
}

fn revalidate_records(
    options: &WorkspaceOptions,
    overlays: &HashMap<Url, OverlayInput>,
    records: &[SourceRecord],
    cancel: &AtomicBool,
    validate_transport_observations: bool,
) -> Result<(), String> {
    for record in records {
        if is_cancelled(cancel) {
            return Err(CANCELLATION_MESSAGE.to_string());
        }
        if let Some(path) = &record.path {
            revalidate_path_record(path, record, cancel, validate_transport_observations)?;
            // A retained path-backed record can become superseded by a newly
            // admitted overlay.  Effective owner validation must compare the
            // overlay's text, while ordinary worker validation must continue
            // to treat the current overlay as authoritative over its disk
            // metadata.
            if let Some(overlay) = overlays.get(&record.uri).filter(|_| {
                !validate_transport_observations
                    && (record.content_hash.is_some() || record.include_payload)
            }) {
                let text_changed =
                    effective_overlay_text_changed(options, path, record, overlay, cancel)?;
                if text_changed {
                    return Err(format!(
                        "source changed while resolving {}; retry the request",
                        record.uri
                    ));
                }
            }
            continue;
        }
        if record.open {
            let Some(overlay) = overlays.get(&record.uri) else {
                return Err(format!(
                    "open document disappeared while resolving {}; retry the request",
                    record.uri
                ));
            };
            let text_changed = record.content_hash.map_or_else(
                || overlay.text != record.text,
                |expected| {
                    if record.text.is_empty() {
                        super::content_hash_bytes(overlay.text.as_bytes()) != expected
                    } else {
                        text_content_hash(&overlay.text) != expected
                    }
                },
            ) || parsed_source_changed(record, &overlay.text);
            if (validate_transport_observations
                && overlay.version != record.version.unwrap_or_default())
                || text_changed
            {
                return Err(format!(
                    "source changed while resolving {}; retry the request",
                    record.uri
                ));
            }
            continue;
        }

        let path = record
            .uri
            .to_file_path()
            .map(absolute_path)
            .map_err(|_| format!("not a file URI: {}", record.uri))?;
        let (read_policy, path_entry) = record.payload_dependency().map_err(|error| {
            format!(
                "closed source changed while resolving {}: {error}",
                record.uri
            )
        })?;
        let allow_legacy_payload =
            matches!(&path_entry.provenance, ProjectPathProvenance::LegacyNative);
        let current = read_disk_source(
            &path,
            options.limits.max_file_bytes,
            read_policy,
            path_entry,
            allow_legacy_payload,
        )
        .map_err(|error| {
            format!(
                "closed source changed while resolving {}: {error}",
                record.uri
            )
        })?;
        let content_changed = record.content_hash.map_or_else(
            || current.text != record.text,
            |expected| expected != current.content_hash,
        ) || parsed_source_changed(record, &current.text);
        let overlay_changed = overlays
            .get(&record.uri)
            .is_some_and(|overlay| overlay.text != current.text);
        if (validate_transport_observations && disk_stamp(&path) != record.stamp)
            || content_changed
            || overlay_changed
        {
            return Err(format!(
                "closed source changed while resolving {}; retry the request",
                record.uri
            ));
        }
    }
    Ok(())
}

fn revalidate_path_record(
    path: &Path,
    record: &SourceRecord,
    cancel: &AtomicBool,
    validate_transport_observations: bool,
) -> Result<(), String> {
    if is_cancelled(cancel) {
        return Err(CANCELLATION_MESSAGE.to_string());
    }
    // A resolver candidate can be rejected for authorization even while the
    // filesystem path exists (for example, a configured symlink). Its
    // presence is not proof that the candidate became readable; the live
    // generation check handles an observed candidate change instead.
    if record.missing_provider_candidate {
        return Ok(());
    }
    if let Some(expected) = &record.candidate_membership {
        let actual =
            pascal_project::project_candidate_membership(path, Some(cancel)).map_err(|error| {
                if error == CANCELLATION_MESSAGE {
                    error
                } else {
                    format!(
                        "project candidate membership could not be revalidated for {}: {error}",
                        path.display()
                    )
                }
            })?;
        if actual != *expected {
            return Err(format!(
                "project candidate membership changed while resolving {}; retry the request",
                path.display()
            ));
        }
    }
    if record.candidate_membership.is_none() || record.path_stamp.is_some() {
        let actual_path_stamp = if is_configuration_file(path) {
            path_stamp_result(path).map_err(|error| {
                format!(
                    "could not inspect configuration candidate {}: {error}",
                    path.display()
                )
            })?
        } else {
            path_stamp(path)
        };
        // Effective validation ignores mtime-only rewrites, but a negative
        // configuration witness must remain a real dependency: absent ->
        // present (or a file/type change) changes the selected context.
        let effective_path_changed = is_configuration_file(path)
            && !effective_path_stamp_matches(&actual_path_stamp, &record.path_stamp);
        if (validate_transport_observations && actual_path_stamp != record.path_stamp)
            || effective_path_changed
        {
            let kind = if is_configuration_file(path) {
                "configuration"
            } else {
                "workspace"
            };
            return Err(format!(
                "{kind} metadata or membership changed while resolving {}; retry the request",
                path.display()
            ));
        }
    }
    if record.candidate_membership.is_some() {
        return Ok(());
    }
    if is_cancelled(cancel) {
        return Err(CANCELLATION_MESSAGE.to_string());
    }
    if let Some(expected) = &record.content_bytes {
        let actual = match read_record_content_bytes(path, record, cancel) {
            Err(error) if error == CANCELLATION_MESSAGE => return Err(error),
            Err(error) => {
                return Err(format!(
                    "workspace content could not be revalidated for {}: {error}",
                    path.display()
                ));
            }
            Ok(actual) => actual,
        };
        if actual != *expected {
            return Err(format!(
                "configuration content changed while resolving {}; retry the request",
                path.display()
            ));
        }
    } else if let Some(expected) = record.content_hash {
        let actual = match read_record_content_hash(path, record, cancel) {
            Err(error) if error == CANCELLATION_MESSAGE => return Err(error),
            Err(error) => {
                return Err(format!(
                    "workspace content could not be revalidated for {}: {error}",
                    path.display()
                ));
            }
            Ok(actual) => actual,
        };
        if expected != actual {
            return Err(format!(
                "workspace metadata changed while resolving {}; retry the request",
                path.display()
            ));
        }
    }
    if is_cancelled(cancel) {
        return Err(CANCELLATION_MESSAGE.to_string());
    }
    Ok(())
}

fn effective_overlay_text_changed(
    options: &WorkspaceOptions,
    path: &Path,
    record: &SourceRecord,
    overlay: &OverlayInput,
    cancel: &AtomicBool,
) -> Result<bool, String> {
    if let Some(expected) = record.parsed_text_hash {
        return Ok(text_content_hash(&overlay.text) != expected);
    }
    if !record.text.is_empty() {
        return Ok(overlay.text != record.text);
    }

    // `content_hash` is deliberately a raw-byte freshness witness.  A
    // Latin-1 disk source and an identical UTF-8 overlay have different raw
    // bytes but the same effective text, so acquire the bounded decoded source
    // when the compact path record did not retain its parsed-text hash.
    let (read_policy, path_entry) = record.payload_dependency().map_err(|error| {
        format!(
            "source text could not be revalidated for {}: {error}",
            path.display()
        )
    })?;
    let allow_legacy_payload =
        matches!(&path_entry.provenance, ProjectPathProvenance::LegacyNative);
    let current = read_disk_source_with_cancel(
        path,
        options.limits.max_file_bytes,
        read_policy,
        path_entry,
        allow_legacy_payload,
        Some(cancel),
    )
    .map_err(|error| {
        format!(
            "source text could not be revalidated for {}: {error}",
            path.display()
        )
    })?;
    if is_cancelled(cancel) {
        return Err(CANCELLATION_MESSAGE.to_string());
    }
    Ok(overlay.text != current.text)
}

fn effective_path_stamp_matches(actual: &Option<PathStamp>, expected: &Option<PathStamp>) -> bool {
    match (actual, expected) {
        (None, None) => true,
        (Some(actual), Some(expected)) => {
            actual.bytes == expected.bytes
                && actual.is_dir == expected.is_dir
                && actual.is_symlink == expected.is_symlink
        }
        _ => false,
    }
}

fn file_content_hash(
    path: &Path,
    read_policy: &ReadPolicy,
    entry: &ProjectPathEntry,
    cancel: &AtomicBool,
) -> Result<u64, String> {
    if is_cancelled(cancel) {
        return Err(CANCELLATION_MESSAGE.to_string());
    }
    let bytes = if matches!(&entry.provenance, ProjectPathProvenance::LegacyNative) {
        read_policy.read_legacy_payload_bytes(entry, MAX_RENAME_SCAN_FILE_BYTES as u64)
    } else {
        read_policy.read_payload_bytes(entry, MAX_RENAME_SCAN_FILE_BYTES as u64)
    }
    .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    if is_cancelled(cancel) {
        return Err(CANCELLATION_MESSAGE.to_string());
    }
    Ok(super::content_hash_bytes(&bytes))
}

pub(crate) fn text_content_hash(source: &str) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    source.hash(&mut hasher);
    hasher.finish()
}

pub(crate) fn parsed_source_changed(record: &SourceRecord, current_text: &str) -> bool {
    record
        .parsed_text_hash
        .is_some_and(|expected| text_content_hash(current_text) != expected)
        || (record.parsed_text_hash.is_none()
            && !record.text.is_empty()
            && current_text != record.text)
}

#[derive(Debug)]
struct ScannedSource {
    data: Vec<u8>,
    bytes: usize,
    content_hash: u64,
}

#[derive(Debug)]
struct AutoImportProviderObservation {
    path: PathBuf,
    content_hash: u64,
    read_policy: ReadPolicy,
    path_entry: ProjectPathEntry,
}

fn read_scan_source(
    path: &Path,
    read_policy: &ReadPolicy,
    entry: &ProjectPathEntry,
    cancel: &AtomicBool,
) -> Result<ScannedSource, String> {
    if is_cancelled(cancel) {
        return Err(CANCELLATION_MESSAGE.to_string());
    }
    let bytes = if matches!(&entry.provenance, ProjectPathProvenance::LegacyNative) {
        read_policy.read_legacy_payload_bytes(entry, MAX_RENAME_SCAN_FILE_BYTES as u64)
    } else {
        read_policy.read_payload_bytes(entry, MAX_RENAME_SCAN_FILE_BYTES as u64)
    }
    .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    let byte_count = bytes.len();
    if is_cancelled(cancel) {
        return Err(CANCELLATION_MESSAGE.to_string());
    }
    if let Some(encoding) = unsupported_source_encoding(&bytes) {
        return Err(format!(
            "{} uses unsupported {encoding} source encoding",
            path.display()
        ));
    }
    let content_hash = super::content_hash_bytes(&bytes);
    Ok(ScannedSource {
        bytes: byte_count,
        data: bytes,
        content_hash,
    })
}

fn unsupported_source_encoding(bytes: &[u8]) -> Option<&'static str> {
    match bytes {
        [0xFF, 0xFE, ..] => Some("UTF-16LE"),
        [0xFE, 0xFF, ..] => Some("UTF-16BE"),
        _ => None,
    }
}

#[allow(clippy::too_many_arguments)]
fn path_record_at(
    path: PathBuf,
    stamp: Option<PathStamp>,
    content_hash: Option<u64>,
    content_bytes: Option<Vec<u8>>,
    candidate_membership: Option<ProjectCandidateMembership>,
    read_policy: Option<ReadPolicy>,
    path_entry: Option<ProjectPathEntry>,
    include_payload: bool,
) -> Option<SourceRecord> {
    let uri = Url::from_file_path(&path).ok()?;
    Some(SourceRecord {
        uri,
        text: String::new(),
        version: None,
        stamp: None,
        open: false,
        path: Some(path),
        path_stamp: stamp,
        content_hash,
        parsed_text_hash: None,
        content_bytes,
        candidate_membership,
        candidate_observations: Vec::new(),
        read_policy,
        path_entry,
        include_payload,
        missing_provider_candidate: false,
        directory_observation: false,
        missing_provider_scope: None,
        auto_import_provider_observation: false,
        auto_import_scopes: Vec::new(),
    })
}

pub(crate) fn snapshot_records(snapshot: &RenameSnapshot) -> Vec<SourceRecord> {
    let mut records = snapshot.records.values().cloned().collect::<Vec<_>>();
    records.extend(snapshot.baseline_records.iter().cloned());
    records
}

pub(crate) fn snapshot_records_bounded(
    snapshot: &RenameSnapshot,
    budget: &mut AssistanceBudget,
    cancel: &AtomicBool,
) -> Result<Vec<SourceRecord>, String> {
    let mut sources = snapshot.records.values().collect::<Vec<_>>();
    budget.require_work(comparison_sort_work(sources.len()), cancel)?;
    sources.sort_by(|left, right| left.uri.as_str().cmp(right.uri.as_str()));
    let mut records = Vec::with_capacity(
        snapshot
            .records
            .len()
            .saturating_add(snapshot.baseline_records.len()),
    );
    budget.require_owned_bytes(
        records
            .capacity()
            .saturating_mul(std::mem::size_of::<SourceRecord>()),
        cancel,
    )?;
    for record in sources.into_iter().chain(snapshot.baseline_records.iter()) {
        if cancel.load(Ordering::Relaxed) {
            return Err(CANCELLATION_MESSAGE.to_string());
        }
        budget.require_work(1, cancel)?;
        let observation_bytes = record
            .candidate_observations
            .iter()
            .map(|observation| observation.path.as_os_str().len())
            .sum::<usize>();
        budget.require_bytes(
            record.uri.as_str().len()
                + record.text.len()
                + record.content_bytes.as_ref().map_or(0, |bytes| bytes.len())
                + observation_bytes,
            cancel,
        )?;
        budget.require_owned_bytes(source_record_owned_bytes(record), cancel)?;
        records.push(record.clone());
    }
    Ok(records)
}

fn comparison_sort_work(length: usize) -> usize {
    if length < 2 {
        return 0;
    }
    let mut remaining = length;
    let mut depth = 0;
    while remaining > 1 {
        depth += 1;
        remaining = remaining.saturating_add(1) / 2;
    }
    length.saturating_mul(depth)
}

pub(crate) fn source_for_input_with_cancel(
    input: &WorkspaceInput,
    uri: &Url,
    cancel: Option<&AtomicBool>,
) -> Result<(String, SourceRecord), String> {
    if cancel.is_some_and(is_cancelled) {
        return Err(CANCELLATION_MESSAGE.to_string());
    }
    let uri = canonical_file_uri(uri);
    if input.rejected_documents.contains(&uri) {
        return Err(format!(
            "document {uri} was rejected and cannot be used for analysis"
        ));
    }
    if let Some(overlay) = input.overlays.get(&uri) {
        return Ok((
            overlay.text.clone(),
            SourceRecord {
                uri,
                text: overlay.text.clone(),
                version: Some(overlay.version),
                stamp: None,
                open: true,
                path: None,
                path_stamp: None,
                content_hash: None,
                parsed_text_hash: Some(text_content_hash(&overlay.text)),
                content_bytes: None,
                candidate_membership: None,
                candidate_observations: Vec::new(),
                read_policy: None,
                path_entry: None,
                include_payload: false,
                missing_provider_candidate: false,
                directory_observation: false,
                missing_provider_scope: None,
                auto_import_provider_observation: false,
                auto_import_scopes: Vec::new(),
            },
        ));
    }

    let fallback_cancel = AtomicBool::new(false);
    let cancel_token = cancel.unwrap_or(&fallback_cancel);
    let owner = owner_for_input(input, &uri, cancel_token)?;
    source_for_input_with_owner(input, &uri, &owner, cancel)
}

pub(crate) fn source_for_input_with_owner(
    input: &WorkspaceInput,
    uri: &Url,
    owner: &KnownDocumentOwner,
    cancel: Option<&AtomicBool>,
) -> Result<(String, SourceRecord), String> {
    let uri = canonical_file_uri(uri);
    if input.rejected_documents.contains(&uri) {
        return Err(format!(
            "document {uri} was rejected and cannot be used for analysis"
        ));
    }
    if let Some(overlay) = input.overlays.get(&uri) {
        return Ok((
            overlay.text.clone(),
            SourceRecord {
                uri,
                text: overlay.text.clone(),
                version: Some(overlay.version),
                stamp: None,
                open: true,
                path: None,
                path_stamp: None,
                content_hash: None,
                parsed_text_hash: Some(text_content_hash(&overlay.text)),
                content_bytes: None,
                candidate_membership: None,
                candidate_observations: Vec::new(),
                read_policy: None,
                path_entry: None,
                include_payload: false,
                missing_provider_candidate: false,
                directory_observation: false,
                missing_provider_scope: None,
                auto_import_provider_observation: false,
                auto_import_scopes: Vec::new(),
            },
        ));
    }
    let path = uri
        .to_file_path()
        .map(absolute_path)
        .map_err(|_| format!("not a file URI: {uri}"))?;
    if has_invalid_project_selection(&owner.state.context) {
        return Err(format!(
            "project selection is invalid; select a current project or Automatic for {uri}"
        ));
    }
    if let Some(error) = owner.state.context.override_error.as_deref() {
        return Err(format!(
            "project override configuration is invalid; analysis is unavailable for {uri}: {error}"
        ));
    }
    let legacy_route = owner.has_legacy_route(&path);
    let entry = super::context_path_entry(&owner.state.context, &path)
        .or_else(|| {
            legacy_route.then_some(ProjectPathEntry {
                path: path.clone(),
                provenance: ProjectPathProvenance::LegacyNative,
            })
        })
        .ok_or_else(|| format!("source is outside the effective project read roots: {uri}"))?;
    let allow_legacy_payload = matches!(&entry.provenance, ProjectPathProvenance::LegacyNative);
    let read_policy = owner.state.context.read_policy.clone();
    let disk = read_disk_source(
        &path,
        input.options.limits.max_file_bytes,
        &read_policy,
        &entry,
        allow_legacy_payload,
    )
    .map_err(|error| format!("could not read source {uri}: {error}"))?;
    if cancel.is_some_and(is_cancelled) {
        return Err(CANCELLATION_MESSAGE.to_string());
    }
    let record = SourceRecord {
        uri: uri.clone(),
        text: disk.text.clone(),
        version: None,
        stamp: Some(disk.stamp),
        open: false,
        path: None,
        path_stamp: None,
        content_hash: Some(disk.content_hash),
        parsed_text_hash: Some(text_content_hash(&disk.text)),
        content_bytes: None,
        candidate_membership: None,
        candidate_observations: Vec::new(),
        read_policy: Some(read_policy),
        path_entry: Some(entry),
        include_payload: false,
        missing_provider_candidate: false,
        directory_observation: false,
        missing_provider_scope: None,
        auto_import_provider_observation: false,
        auto_import_scopes: Vec::new(),
    };
    Ok((disk.text, record))
}

#[allow(dead_code)]
pub(crate) fn source_for_input(
    input: &WorkspaceInput,
    uri: &Url,
) -> Result<(String, SourceRecord), String> {
    source_for_input_with_cancel(input, uri, None)
}

pub(crate) fn input_source_is_editable(input: &WorkspaceInput, uri: &Url) -> bool {
    let workspace = Workspace::with_override_session(
        input.roots.clone(),
        input.options.clone(),
        input.overrides.clone(),
    );
    let Ok(path) = uri.to_file_path() else {
        return false;
    };
    let path = absolute_path(path);
    workspace.accepts_path(&path) && is_editable_source_path(&workspace, &path)
}

/// Prove that a source action can target the physical path under the current
/// requester policy. An open overlay for a not-yet-created source remains
/// eligible: the client owns that unsaved buffer and no filesystem permission
/// proof exists to inspect. Existing physical files must explicitly expose a
/// writable permission bit; unknown metadata/errors fail closed.
pub(crate) fn input_source_is_writable(input: &WorkspaceInput, uri: &Url) -> bool {
    let workspace = Workspace::with_override_session(
        input.roots.clone(),
        input.options.clone(),
        input.overrides.clone(),
    );
    let Ok(path) = uri.to_file_path() else {
        return false;
    };
    let path = absolute_path(path);
    if !workspace.accepts_path(&path) || !is_editable_source_path(&workspace, &path) {
        return false;
    }
    match fs::symlink_metadata(&path) {
        Ok(_) => match fs::metadata(&path) {
            Ok(metadata) => {
                if !metadata.is_file() {
                    return false;
                }
                #[cfg(unix)]
                {
                    effective_unix_write_access(&path, &metadata)
                }
                #[cfg(not(unix))]
                {
                    !metadata.permissions().readonly()
                }
            }
            Err(_) => false,
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            input.overlays.contains_key(&canonical_file_uri(uri))
        }
        Err(_) => false,
    }
}

#[cfg(unix)]
fn effective_unix_write_access(path: &Path, metadata: &fs::Metadata) -> bool {
    let effective_uid = unsafe { libc::geteuid() };
    let effective_gid = unsafe { libc::getegid() };
    let owner = metadata.uid() == effective_uid;
    let group = metadata.gid() == effective_gid || effective_groups_contain(metadata.gid());
    let mode = metadata.permissions().mode();
    let class_allows = if owner {
        mode & 0o200 != 0
    } else if group {
        mode & 0o020 != 0
    } else {
        mode & 0o002 != 0
    };

    // A privileged process must not infer ordinary-user writability from its
    // ability to bypass mode checks. The owner/group/other class is the
    // conservative baseline; ACL-aware faccessat may widen it for an
    // unprivileged caller when the platform can prove that access.
    if !class_allows && effective_uid == 0 {
        return false;
    }
    let Ok(path) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    let access_ok = unsafe {
        libc::faccessat(libc::AT_FDCWD, path.as_ptr(), libc::W_OK, libc::AT_EACCESS) == 0
    };
    (effective_uid != 0 || class_allows) && access_ok
}

#[cfg(unix)]
fn effective_groups_contain(gid: u32) -> bool {
    let count = unsafe { libc::getgroups(0, std::ptr::null_mut()) };
    if count < 0 {
        return false;
    }
    let mut groups = vec![0 as libc::gid_t; count as usize];
    let filled = unsafe { libc::getgroups(count, groups.as_mut_ptr()) };
    filled >= 0 && groups[..filled as usize].contains(&(gid as libc::gid_t))
}

pub(crate) fn owner_for_input(
    input: &WorkspaceInput,
    uri: &Url,
    cancel: &AtomicBool,
) -> Result<KnownDocumentOwner, String> {
    let uri = canonical_file_uri(uri);
    let owner_origin = input.document_owners.get(&uri).map(|owner| owner.origin);

    let mut workspace = Workspace::from_analysis_input(input);
    let key = workspace.context_for_uri_with_cancel(&uri, Some(cancel))?;
    let state = workspace
        .contexts
        .get(&key)
        .cloned()
        .ok_or_else(|| format!("project context was not retained for {uri}"))?;
    Ok(KnownDocumentOwner {
        key: key.clone(),
        state,
        origin: owner_origin.unwrap_or_else(|| workspace.owner_origin_for_context_key(&key)),
        needs_revalidation: false,
        follow_current_project_file: false,
        legacy_route: workspace.document_owners.get(&uri).and_then(|owner| {
            (owner.key == key)
                .then(|| owner.legacy_route.clone())
                .flatten()
        }),
    })
}

pub(crate) fn input_source_is_readable_with_owner(
    input: &WorkspaceInput,
    uri: &Url,
    owner: &KnownDocumentOwner,
) -> bool {
    let workspace = Workspace::with_override_session(
        input.roots.clone(),
        input.options.clone(),
        input.overrides.clone(),
    );
    let Ok(path) = uri.to_file_path() else {
        return false;
    };
    let path = absolute_path(path);
    let legacy_route = owner.has_legacy_route(&path);
    workspace.ensure_supported_project_context_with_legacy_route(
        &path,
        &owner.state.context,
        Some(&owner.key),
        legacy_route,
    ) && (workspace.accepts_path(&path)
        || owner.state.context.project_file.is_some()
        || workspace.mapped_path_is_readable(&path, &owner.key)
        || legacy_route)
}

#[derive(Debug)]
pub(crate) struct BindingClassification {
    pub(crate) source: String,
    pub(crate) record: SourceRecord,
    pub(crate) info: Option<(crate::navigation::RenameBindingInfo, bool)>,
    pub(crate) ignored_or_empty: bool,
    pub(crate) consumed_configuration: Vec<SourceRecord>,
}

type ExpandedBindingInfo = Option<(Option<(crate::navigation::RenameBindingInfo, bool)>, bool)>;

pub(crate) fn binding_info_for_input(
    input: &WorkspaceInput,
    uri: &Url,
    position: Position,
    additional_names: &[String],
    cancel: &AtomicBool,
) -> Result<BindingClassification, String> {
    binding_classification_for_input(
        input,
        uri,
        position,
        additional_names,
        SelfContainedMode::AnyBinding,
        cancel,
    )
}

pub(crate) fn query_binding_info_for_input(
    input: &WorkspaceInput,
    uri: &Url,
    position: Position,
    cancel: &AtomicBool,
) -> Result<BindingClassification, String> {
    binding_classification_for_input(input, uri, position, &[], SelfContainedMode::None, cancel)
}

pub(crate) fn reference_binding_info_for_input(
    input: &WorkspaceInput,
    uri: &Url,
    position: Position,
    cancel: &AtomicBool,
) -> Result<BindingClassification, String> {
    binding_classification_for_input(
        input,
        uri,
        position,
        &[],
        SelfContainedMode::LocalBinding,
        cancel,
    )
}

#[derive(Debug, Clone, Copy)]
enum SelfContainedMode {
    None,
    AnyBinding,
    LocalBinding,
}

fn binding_classification_for_input(
    input: &WorkspaceInput,
    uri: &Url,
    position: Position,
    additional_names: &[String],
    self_contained_mode: SelfContainedMode,
    cancel: &AtomicBool,
) -> Result<BindingClassification, String> {
    let (source, record) = source_for_input_with_cancel(input, uri, Some(cancel))?;
    let (context, consumed_configuration) =
        project_context_and_metadata_for_input(input, uri, cancel)?;
    // An incomplete project context cannot establish conditional branch facts.
    // Self-contained classification must therefore prove the local binding
    // without inheriting defines from an ambiguous or partially read project.
    let conditional_context =
        if !context.discovery_complete && !matches!(self_contained_mode, SelfContainedMode::None) {
            ConditionalContext::default()
        } else {
            context.effective_conditional_context()
        };
    let expanded_info = expanded_binding_info_for_input(
        input,
        uri,
        position,
        additional_names,
        self_contained_mode,
        cancel,
    )?;
    let (mut info, ignored_or_empty) = expanded_info.unwrap_or(binding_info_for_source(
        uri,
        &source,
        position,
        additional_names,
        &conditional_context,
        self_contained_mode,
        cancel,
    )?);
    if let Some((info, _)) = info.as_mut() {
        add_project_unit_alias_names(info, &context);
    }
    Ok(BindingClassification {
        source,
        record,
        info,
        ignored_or_empty,
        consumed_configuration,
    })
}

fn add_project_unit_alias_names(
    info: &mut crate::navigation::RenameBindingInfo,
    context: &ProjectContext,
) {
    if !info.unit {
        return;
    }
    let names = info
        .names
        .iter()
        .map(|name| name.trim_start_matches('&').to_ascii_lowercase())
        .collect::<HashSet<_>>();
    for (alias, target) in &context.unit_aliases {
        if names.contains(&target.trim_start_matches('&').to_ascii_lowercase()) {
            info.names.push(alias.clone());
        }
    }
    info.names.sort_by_key(|name| name.to_ascii_lowercase());
    info.names
        .dedup_by(|left, right| left.eq_ignore_ascii_case(right));
}

fn expanded_binding_info_for_input(
    input: &WorkspaceInput,
    uri: &Url,
    position: Position,
    additional_names: &[String],
    self_contained_mode: SelfContainedMode,
    cancel: &AtomicBool,
) -> Result<ExpandedBindingInfo, String> {
    let uri = canonical_file_uri(uri);
    let mut workspace = Workspace::from_analysis_input(input);
    let context_key = workspace.context_for_uri_with_cancel(&uri, Some(cancel))?;
    if !workspace.load_source_with_cancel(&uri, &context_key, &HashSet::new(), Some(cancel))? {
        return Ok(None);
    }
    let mut mapping_budget = include_expansion::MappingBudget::new(
        cancel,
        workspace.include_expansion_limits().max_work,
    );
    let positions =
        workspace.virtual_query_positions_with_budget(&uri, position, &mut mapping_budget)?;
    if positions.is_empty() {
        // An incomplete expansion must not be mistaken for a harmless
        // whitespace/comment position.  The physical target may be valid,
        // but refusing to classify it lets the caller's bounded snapshot
        // report the include/conditional incompleteness instead of silently
        // returning no references or highlights.
        let incomplete_mapping = workspace
            .source_text_for_mapping(&uri)
            .and_then(|source| {
                let offset = text::position_to_offset(&source, position)?;
                let width = source
                    .get(offset..)
                    .and_then(|tail| tail.chars().next())
                    .map_or(1, char::len_utf8);
                Some(workspace.expansions.values().any(|expansion| {
                    let reverse = expansion.expanded.reverse_range_with_budget(
                        &uri,
                        offset..offset.saturating_add(width),
                        &mut mapping_budget,
                    );
                    !expansion.complete && reverse.map(|ranges| !ranges.is_empty()).unwrap_or(false)
                }))
            })
            .unwrap_or(false);
        if incomplete_mapping {
            return Ok(Some((None, false)));
        }
        return Ok(Some((None, true)));
    }

    let mut selected: Option<(crate::navigation::RenameBindingInfo, bool)> = None;
    let mut ignored = false;
    for (query_uri, query_position) in positions {
        if is_cancelled(cancel) {
            return Err(CANCELLATION_MESSAGE.to_string());
        }
        let query_ignored = workspace
            .index
            .position_is_ignored_or_empty(&query_uri, query_position)?;
        if query_ignored {
            ignored = true;
            continue;
        }
        let Some(info) = workspace
            .index
            .rename_binding_info_with_cancel(&query_uri, query_position, cancel)
            .ok()
        else {
            return Ok(Some((None, false)));
        };
        let can_check_self_contained = match self_contained_mode {
            SelfContainedMode::None => false,
            SelfContainedMode::AnyBinding => true,
            SelfContainedMode::LocalBinding => info.local,
        };
        let self_contained = can_check_self_contained
            && workspace.index.self_contained_rename_binding_with_cancel(
                &query_uri,
                query_position,
                additional_names,
                cancel,
            );
        let current = (info, self_contained);
        if selected.as_ref().is_some_and(|previous| {
            previous.1 != current.1
                || previous.0.local != current.0.local
                || previous.0.names.iter().collect::<HashSet<_>>()
                    != current.0.names.iter().collect::<HashSet<_>>()
        }) {
            return Ok(Some((None, false)));
        }
        selected = Some(current);
    }
    if selected.is_none() && ignored {
        return Ok(Some((None, true)));
    }
    Ok(Some((selected, ignored)))
}

pub(crate) fn project_context_and_metadata_for_input(
    input: &WorkspaceInput,
    uri: &Url,
    cancel: &AtomicBool,
) -> Result<(ProjectContext, Vec<SourceRecord>), String> {
    let mut workspace = Workspace::from_analysis_input(input);
    let context_key = workspace.context_for_uri_with_cancel(uri, Some(cancel))?;
    let state = workspace
        .contexts
        .get(&context_key)
        .cloned()
        .ok_or_else(|| format!("project context was not retained for {uri}"))?;
    let records = consumed_context_records(&state, cancel)?;
    Ok((state.context, records))
}

pub(crate) fn project_context_and_metadata_for_owner(
    owner: &KnownDocumentOwner,
    cancel: &AtomicBool,
) -> Result<(ProjectContext, Vec<SourceRecord>), String> {
    let records = consumed_context_records(&owner.state, cancel)?;
    Ok((owner.state.context.clone(), records))
}

pub(crate) fn consumed_context_records(
    state: &super::ContextState,
    cancel: &AtomicBool,
) -> Result<Vec<SourceRecord>, String> {
    let mut metadata_paths = state.context.metadata_files.clone();
    if let Some(project_file) = &state.context.project_file {
        metadata_paths.push(project_file.clone());
    }
    if let Some(main_source) = &state.context.main_source {
        metadata_paths.push(main_source.clone());
    }
    metadata_paths.sort_by(|left, right| left.to_string_lossy().cmp(&right.to_string_lossy()));
    metadata_paths.dedup_by(|left, right| path_key(left) == path_key(right));

    let mut records = Vec::new();
    let mut seen = HashSet::new();
    for path in metadata_paths {
        if !seen.insert(path_key(&path)) {
            continue;
        }
        if is_cancelled(cancel) {
            return Err(CANCELLATION_MESSAGE.to_string());
        }
        let observed = state
            .project_read_observations
            .iter()
            .find(|observation| path_key(&observation.path) == path_key(&path));
        let stamp = observed
            .map(|observation| Some(super::path_stamp_from_project_read(&observation.stamp)))
            .or_else(|| state.watched_paths.get(&path).cloned())
            .ok_or_else(|| {
                format!(
                    "project metadata observation was not retained for {}",
                    path.display()
                )
            })?;
        let content_hash = observed.map(|observation| observation.content_hash);
        let content_bytes = observed.and_then(|observation| observation.content_bytes.clone());
        let payload = state
            .context
            .metadata_observations
            .iter()
            .find(|observation| path_key(observation.path()) == path_key(&path));
        let (read_policy, path_entry) = match payload {
            Some(MetadataObservation::Payload {
                read_policy,
                path_entry,
                ..
            }) => (Some(read_policy.clone()), Some(path_entry.clone())),
            Some(MetadataObservation::Stat { .. }) | None => (None, None),
        };
        let missing_provider_candidate = stamp.is_none() && !is_configuration_file(&path);
        if let Some(mut record) = path_record_at(
            path,
            stamp,
            content_hash,
            content_bytes,
            None,
            read_policy,
            path_entry,
            false,
        ) {
            record.missing_provider_candidate = missing_provider_candidate;
            records.push(record);
        }
    }

    let mut memberships = state
        .project_candidate_memberships
        .iter()
        .collect::<Vec<_>>();
    memberships
        .sort_by(|(left, _), (right, _)| left.to_string_lossy().cmp(&right.to_string_lossy()));
    for (directory, membership) in memberships {
        if is_cancelled(cancel) {
            return Err(CANCELLATION_MESSAGE.to_string());
        }
        let membership = match membership {
            Ok(membership) => membership.clone(),
            Err(error) if error == CANCELLATION_MESSAGE => return Err(error.clone()),
            Err(error) => {
                return Err(format!(
                    "could not observe project candidates in {}: {error}",
                    directory.display()
                ));
            }
        };
        if seen.insert(path_key(directory)) {
            if let Some(record) = path_record_at(
                directory.clone(),
                None,
                None,
                None,
                Some(membership),
                None,
                None,
                false,
            ) {
                records.push(record);
            }
        }
    }

    Ok(records)
}

fn binding_info_for_source(
    uri: &Url,
    source: &str,
    position: Position,
    additional_names: &[String],
    conditional_context: &ConditionalContext,
    self_contained_mode: SelfContainedMode,
    cancel: &AtomicBool,
) -> Result<(Option<(crate::navigation::RenameBindingInfo, bool)>, bool), String> {
    let uri = canonical_file_uri(uri);
    let mut index = NavigationIndex::new();
    index
        .update_with_context_with_cancel(
            uri.clone(),
            source.to_owned(),
            conditional_context,
            cancel,
        )
        .map_err(|error| format!("could not index rename source {uri}: {error}"))?;
    let ignored_or_empty = index.position_is_ignored_or_empty(&uri, position)?;
    let info = if ignored_or_empty {
        None
    } else {
        index
            .rename_binding_info_with_cancel(&uri, position, cancel)
            .ok()
    };
    let can_check_self_contained = match self_contained_mode {
        SelfContainedMode::None => false,
        SelfContainedMode::AnyBinding => true,
        SelfContainedMode::LocalBinding => info.as_ref().is_some_and(|info| info.local),
    };
    let self_contained = can_check_self_contained
        && index.self_contained_rename_binding_with_cancel(
            &uri,
            position,
            additional_names,
            cancel,
        );
    Ok((info.map(|info| (info, self_contained)), ignored_or_empty))
}

fn contains_any_identifier(source: &str, names: &[String]) -> bool {
    let bytes = source.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        while index < bytes.len() && !is_identifier_byte(bytes[index]) {
            index += 1;
        }
        let start = index;
        while index < bytes.len() && is_identifier_byte(bytes[index]) {
            index += 1;
        }
        if start == index {
            continue;
        }
        let Some(identifier) = source.get(start..index) else {
            continue;
        };
        if names
            .iter()
            .any(|name| identifier.eq_ignore_ascii_case(name.trim_start_matches('&')))
        {
            return true;
        }
    }
    false
}

fn inherited_conditional_branch_contains_identifier(
    source: &str,
    analysis: &conditional::ConditionalAnalysis,
    names: &[String],
) -> bool {
    let mut open_conditions = Vec::new();
    let mut conditional_ranges = Vec::new();
    for directive in &analysis.directives {
        match directive.kind {
            ConditionalDirectiveKind::ConditionalStart => open_conditions.push(directive.end),
            ConditionalDirectiveKind::ConditionalEnd => {
                if let Some(start) = open_conditions.pop() {
                    conditional_ranges.push(start..directive.start);
                }
            }
            _ => {}
        }
    }
    if conditional_ranges.is_empty() {
        return false;
    }

    let bytes = source.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        while index < bytes.len() && !is_identifier_byte(bytes[index]) {
            index += 1;
        }
        let start = index;
        while index < bytes.len() && is_identifier_byte(bytes[index]) {
            index += 1;
        }
        if start == index {
            continue;
        }
        if analysis
            .directives
            .iter()
            .any(|directive| start >= directive.start && index <= directive.end)
        {
            continue;
        }
        let Some(identifier) = source.get(start..index) else {
            continue;
        };
        if names
            .iter()
            .any(|name| identifier.eq_ignore_ascii_case(name.trim_start_matches('&')))
            && conditional_ranges
                .iter()
                .any(|range| start >= range.start && index <= range.end)
        {
            return true;
        }
    }
    false
}

fn contains_any_identifier_bytes(source: &[u8], names: &[String]) -> bool {
    let mut index = 0;
    while index < source.len() {
        while index < source.len() && !is_identifier_byte(source[index]) {
            index += 1;
        }
        let start = index;
        while index < source.len() && is_identifier_byte(source[index]) {
            index += 1;
        }
        if start == index {
            continue;
        }
        let identifier = &source[start..index];
        if names
            .iter()
            .any(|name| identifier.eq_ignore_ascii_case(name.trim_start_matches('&').as_bytes()))
        {
            return true;
        }
    }
    false
}

pub(crate) fn contains_any_identifier_prefix(source: &str, names: &[String]) -> bool {
    let bytes = source.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        while index < bytes.len() && !is_identifier_byte(bytes[index]) {
            index += 1;
        }
        let start = index;
        while index < bytes.len() && is_identifier_byte(bytes[index]) {
            index += 1;
        }
        if start == index {
            continue;
        }
        let Some(identifier) = source.get(start..index) else {
            continue;
        };
        if names.iter().any(|name| {
            let name = name.trim_start_matches('&');
            !name.is_empty()
                && identifier.len() >= name.len()
                && identifier[..name.len()].eq_ignore_ascii_case(name)
        }) {
            return true;
        }
    }
    false
}

fn contains_any_identifier_prefix_bytes(source: &[u8], names: &[String]) -> bool {
    let mut index = 0;
    while index < source.len() {
        while index < source.len() && !is_identifier_byte(source[index]) {
            index += 1;
        }
        let start = index;
        while index < source.len() && is_identifier_byte(source[index]) {
            index += 1;
        }
        if start == index {
            continue;
        }
        let identifier = &source[start..index];
        if names.iter().any(|name| {
            let name = name.trim_start_matches('&').as_bytes();
            !name.is_empty()
                && identifier.len() >= name.len()
                && identifier[..name.len()].eq_ignore_ascii_case(name)
        }) {
            return true;
        }
    }
    false
}

pub(crate) fn source_unit_name(
    source: &str,
    cancel: &AtomicBool,
) -> Result<Option<String>, String> {
    let bytes = source.as_bytes();
    let mut cursor = 0usize;
    let Some(keyword) = next_source_identifier(source, &mut cursor, cancel)? else {
        return Ok(None);
    };
    if !keyword.eq_ignore_ascii_case("unit") {
        return Ok(None);
    }
    let Some(first) = next_source_identifier(source, &mut cursor, cancel)? else {
        return Ok(None);
    };
    let mut parts = vec![first];
    loop {
        if is_cancelled(cancel) {
            return Err(CANCELLATION_MESSAGE.to_string());
        }
        while bytes.get(cursor).is_some_and(u8::is_ascii_whitespace) {
            cursor += 1;
        }
        if bytes.get(cursor) != Some(&b'.') {
            break;
        }
        cursor += 1;
        let Some(part) = next_source_identifier(source, &mut cursor, cancel)? else {
            return Ok(None);
        };
        parts.push(part);
    }
    let name = parts
        .into_iter()
        .map(|part| part.trim_start_matches('&').to_ascii_lowercase())
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(".");
    Ok((!name.is_empty()).then_some(name))
}

fn next_source_identifier(
    source: &str,
    cursor: &mut usize,
    cancel: &AtomicBool,
) -> Result<Option<String>, String> {
    let bytes = source.as_bytes();
    loop {
        if is_cancelled(cancel) {
            return Err(CANCELLATION_MESSAGE.to_string());
        }
        while bytes
            .get(*cursor)
            .is_some_and(|byte| byte.is_ascii_whitespace())
        {
            *cursor += 1;
        }
        if bytes.get(*cursor) == Some(&b'/') && bytes.get(*cursor + 1) == Some(&b'/') {
            *cursor += 2;
            while *cursor < bytes.len() && bytes[*cursor] != b'\r' && bytes[*cursor] != b'\n' {
                *cursor += 1;
            }
            continue;
        }
        if bytes.get(*cursor) == Some(&b'{') {
            *cursor += 1;
            while *cursor < bytes.len() && bytes[*cursor] != b'}' {
                *cursor += 1;
            }
            if *cursor == bytes.len() {
                return Ok(None);
            }
            *cursor += 1;
            continue;
        }
        if bytes.get(*cursor) == Some(&b'(') && bytes.get(*cursor + 1) == Some(&b'*') {
            *cursor += 2;
            while *cursor + 1 < bytes.len()
                && !(bytes[*cursor] == b'*' && bytes[*cursor + 1] == b')')
            {
                *cursor += 1;
            }
            if *cursor + 1 >= bytes.len() {
                return Ok(None);
            }
            *cursor += 2;
            continue;
        }
        if bytes.get(*cursor) == Some(&b'\'') {
            *cursor += 1;
            while *cursor < bytes.len() {
                if bytes[*cursor] == b'\'' {
                    if bytes.get(*cursor + 1) == Some(&b'\'') {
                        *cursor += 2;
                    } else {
                        *cursor += 1;
                        break;
                    }
                } else {
                    *cursor += 1;
                }
            }
            continue;
        }
        let Some(&byte) = bytes.get(*cursor) else {
            return Ok(None);
        };
        if byte.is_ascii_alphabetic() || byte == b'_' || byte == b'&' {
            let start = *cursor;
            *cursor += 1;
            while bytes
                .get(*cursor)
                .is_some_and(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
            {
                *cursor += 1;
            }
            return Ok(source.get(start..*cursor).map(str::to_owned));
        }
        return Ok(None);
    }
}

pub(crate) fn may_contain_include_directive(source: &[u8]) -> bool {
    source.windows(2).any(|window| window == b"{$")
        || source.windows(3).any(|window| window == b"(*$")
}

fn source_requires_include_owner_closure(uri: &Url, source: &str) -> bool {
    uri.to_file_path().ok().is_some_and(|path| {
        path.extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("inc"))
    }) || may_contain_include_directive(source.as_bytes())
}

fn is_identifier_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

pub(crate) fn prepare_from_input(
    input: WorkspaceInput,
    uri: &Url,
    position: Position,
    cancel: &AtomicBool,
) -> Computed<PrepareRenameResponse> {
    let source_generation = input.source_generation;
    let configuration_generation = input.configuration_generation;
    let uri = canonical_file_uri(uri);
    let (initial_source, _) = match source_for_input_with_cancel(&input, &uri, Some(cancel)) {
        Ok(source) => source,
        Err(error) => return failed(source_generation, configuration_generation, error),
    };
    let original_name = match identifier_at_position(&initial_source, position) {
        Some(name) => name,
        None => {
            return failed(
                source_generation,
                configuration_generation,
                format!("no identifier at rename position in {uri}"),
            );
        }
    };
    let classification = match binding_info_for_input(&input, &uri, position, &[], cancel) {
        Ok(result) => result,
        Err(error) => return failed(source_generation, configuration_generation, error),
    };
    let BindingClassification {
        source: planning_source,
        record: target_record,
        info: binding_info,
        consumed_configuration,
        ..
    } = classification;
    if initial_source != planning_source {
        return failed(
            source_generation,
            configuration_generation,
            "rename target source changed while classifying; retry the request".to_string(),
        );
    }
    let include_context = source_requires_include_owner_closure(&uri, &planning_source);
    let (mode, candidate_names, self_contained) = match binding_info {
        Some((info, self_contained)) => {
            let mut names = info.names;
            if names.is_empty() {
                names.push(original_name.clone());
            }
            (
                if info.local && !include_context {
                    SnapshotMode::Local
                } else {
                    SnapshotMode::Workspace
                },
                names,
                self_contained,
            )
        }
        None => (SnapshotMode::Workspace, vec![original_name], false),
    };
    let skip_imports_for: &[Url] = if self_contained {
        std::slice::from_ref(&uri)
    } else {
        &[]
    };
    let snapshot = match build_snapshot(
        &input,
        std::slice::from_ref(&uri),
        &candidate_names,
        mode,
        Some(SnapshotSeed::new(target_record).with_consumed_configuration(&consumed_configuration)),
        skip_imports_for,
        cancel,
    ) {
        Ok(snapshot) => snapshot,
        Err(error) => {
            return Computed {
                source_generation,
                configuration_generation,
                value: Err(error),
                records: Vec::new(),
            };
        }
    };
    if let Err(error) = ensure_ready(&snapshot, &uri) {
        return Computed {
            source_generation,
            configuration_generation,
            value: Err(error),
            records: Vec::new(),
        };
    }
    if is_cancelled(cancel) {
        return cancelled(source_generation, configuration_generation);
    }

    if let Err(error) = check_includes(&snapshot, &uri, position, &candidate_names, Some(cancel)) {
        return Computed {
            source_generation,
            configuration_generation,
            value: Err(error),
            records: Vec::new(),
        };
    }
    let value = snapshot.prepare_rename(&uri, position, cancel);
    let records = snapshot_records(&snapshot);
    Computed {
        source_generation,
        configuration_generation,
        value,
        records,
    }
}

pub(crate) fn rename_from_input(
    input: WorkspaceInput,
    uri: &Url,
    position: Position,
    new_name: &str,
    document_changes: bool,
    cancel: &AtomicBool,
) -> Computed<WorkspaceEdit> {
    let source_generation = input.source_generation;
    let configuration_generation = input.configuration_generation;
    let uri = canonical_file_uri(uri);
    let (initial_source, _) = match source_for_input_with_cancel(&input, &uri, Some(cancel)) {
        Ok(source) => source,
        Err(error) => return failed(source_generation, configuration_generation, error),
    };
    let initial_disk_stamp = uri
        .to_file_path()
        .ok()
        .map(absolute_path)
        .and_then(|path| disk_stamp(&path));
    let original_name = match identifier_at_position(&initial_source, position) {
        Some(name) => name,
        None => {
            return failed(
                source_generation,
                configuration_generation,
                format!("no identifier at rename position in {uri}"),
            );
        }
    };
    let additional_names = [new_name.to_owned()];
    let classification =
        match binding_info_for_input(&input, &uri, position, &additional_names, cancel) {
            Ok(result) => result,
            Err(error) => {
                // The scope-classification read is deliberately separate from
                // the initial target read.  If the target changed between
                // those reads, report the stale-source condition even when
                // the new contents no longer contain an identifier at the
                // original position.
                let stamp_changed = uri
                    .to_file_path()
                    .ok()
                    .map(absolute_path)
                    .and_then(|path| disk_stamp(&path))
                    != initial_disk_stamp;
                if stamp_changed
                    || error == "no renameable identifier at position"
                    || source_for_input_with_cancel(&input, &uri, Some(cancel))
                        .is_ok_and(|(current, _)| current != initial_source)
                {
                    return failed(
                        source_generation,
                        configuration_generation,
                        format!("source changed while classifying rename target {uri}"),
                    );
                }
                return failed(source_generation, configuration_generation, error);
            }
        };
    let BindingClassification {
        source: planning_source,
        record: target_record,
        info: binding_info,
        consumed_configuration,
        ..
    } = classification;
    if initial_source != planning_source {
        return failed(
            source_generation,
            configuration_generation,
            "rename target source changed while classifying; retry the request".to_string(),
        );
    }
    let include_context = source_requires_include_owner_closure(&uri, &planning_source);
    let (mode, candidate_names, self_contained) = match binding_info {
        Some((info, self_contained)) => {
            let mut names = info.names;
            if names.is_empty() {
                names.push(original_name.clone());
            }
            names.push(new_name.to_string());
            (
                if info.local && !include_context {
                    SnapshotMode::Local
                } else {
                    SnapshotMode::Workspace
                },
                names,
                self_contained,
            )
        }
        None => (
            SnapshotMode::Workspace,
            vec![original_name, new_name.to_string()],
            false,
        ),
    };
    let skip_imports_for: &[Url] = if self_contained {
        std::slice::from_ref(&uri)
    } else {
        &[]
    };
    let snapshot = match build_snapshot(
        &input,
        std::slice::from_ref(&uri),
        &candidate_names,
        mode,
        Some(SnapshotSeed::new(target_record).with_consumed_configuration(&consumed_configuration)),
        skip_imports_for,
        cancel,
    ) {
        Ok(snapshot) => snapshot,
        Err(error) => {
            return Computed {
                source_generation,
                configuration_generation,
                value: Err(error),
                records: Vec::new(),
            };
        }
    };
    if let Err(error) = ensure_ready(&snapshot, &uri) {
        return Computed {
            source_generation,
            configuration_generation,
            value: Err(error),
            records: Vec::new(),
        };
    }
    if is_cancelled(cancel) {
        return cancelled(source_generation, configuration_generation);
    }

    let raw_edits = match snapshot.rename_edits(&uri, position, new_name, cancel) {
        Ok(edits) => edits,
        Err(error) => {
            if error == "no renameable identifier at position" {
                return Computed {
                    source_generation,
                    configuration_generation,
                    value: Err(format!(
                        "source changed while resolving rename target {uri}"
                    )),
                    records: Vec::new(),
                };
            }
            let include_sensitive = snapshot
                .sources
                .values()
                .any(|source| may_contain_include_directive(source.as_bytes()))
                || snapshot
                    .expansions
                    .values()
                    .any(|expansion| !expansion.dependencies.is_empty());
            let error = if include_sensitive {
                format!("include-expanded rename could not resolve a complete binding: {error}")
            } else {
                error
            };
            return Computed {
                source_generation,
                configuration_generation,
                value: Err(error),
                records: Vec::new(),
            };
        }
    };
    if let Err(error) = check_includes(&snapshot, &uri, position, &candidate_names, Some(cancel)) {
        return Computed {
            source_generation,
            configuration_generation,
            value: Err(error),
            records: Vec::new(),
        };
    }
    for edited_uri in raw_edits.keys() {
        if !snapshot.editable.contains(edited_uri) {
            return Computed {
                source_generation,
                configuration_generation,
                value: Err(format!(
                    "rename would modify source outside configured workspace roots: {edited_uri}"
                )),
                records: Vec::new(),
            };
        }
        if !snapshot.records.contains_key(edited_uri) {
            return Computed {
                source_generation,
                configuration_generation,
                value: Err(format!(
                    "rename source was not retained in the complete workspace snapshot: {edited_uri}"
                )),
                records: Vec::new(),
            };
        }
    }
    if is_cancelled(cancel) {
        return cancelled(source_generation, configuration_generation);
    }
    let records = snapshot_records(&snapshot);
    let value = workspace_edit(raw_edits, &snapshot.records, document_changes);
    Computed {
        source_generation,
        configuration_generation,
        value,
        records,
    }
}

fn cancelled<T>(source_generation: u64, configuration_generation: u64) -> Computed<T> {
    Computed {
        source_generation,
        configuration_generation,
        value: Err(CANCELLATION_MESSAGE.to_string()),
        records: Vec::new(),
    }
}

fn failed<T>(source_generation: u64, configuration_generation: u64, error: String) -> Computed<T> {
    Computed {
        source_generation,
        configuration_generation,
        value: Err(error),
        records: Vec::new(),
    }
}

pub(crate) fn is_cancelled(cancel: &AtomicBool) -> bool {
    cancel.load(Ordering::Relaxed)
}

fn snapshot_context_for_uri(
    loader: &mut Workspace,
    uri: &Url,
    cancel: &AtomicBool,
) -> Result<ContextKey, String> {
    if let Some(owner) = loader.document_owners.get(uri).cloned() {
        if loader.context_has_open_legacy_overlay(&owner.state)
            || (!super::context_state_is_fresh_with_cancel(&owner.state, Some(cancel))?
                && loader.context_state_is_fresh_with_open_documents(&owner.state, Some(cancel))?)
        {
            loader
                .contexts
                .insert(owner.key.clone(), owner.state.clone());
            loader
                .document_contexts
                .insert(uri.clone(), owner.key.clone());
        }
    }
    loader.context_for_uri_with_cancel(uri, Some(cancel))
}

fn snapshot_payload_dependency(
    loader: &Workspace,
    context_key: &ContextKey,
    path: &Path,
) -> Result<(ReadPolicy, ProjectPathEntry), String> {
    let context = loader
        .contexts
        .get(context_key)
        .map(|state| &state.context)
        .ok_or_else(|| format!("project context was not retained for {path:?}"))?;
    let uri = Url::from_file_path(path)
        .map_err(|()| format!("could not create a file URI for {path:?}"))?;
    let legacy_route = loader.legacy_route_is_current(&uri, path, context_key);
    let entry = super::context_path_entry(context, path)
        .or_else(|| {
            legacy_route.then_some(ProjectPathEntry {
                path: path.to_path_buf(),
                provenance: ProjectPathProvenance::LegacyNative,
            })
        })
        .ok_or_else(|| {
            format!("source path is outside the effective project read roots: {path:?}")
        })?;
    if !loader.ensure_supported_project_context_with_legacy_route(
        path,
        context,
        Some(context_key),
        legacy_route,
    ) {
        return Err(format!(
            "source path is outside the effective project read roots: {path:?}"
        ));
    }
    Ok((context.read_policy.clone(), entry))
}

fn discover_enumerated_contexts(
    loader: &mut Workspace,
    input: &WorkspaceInput,
    enumeration: &mut Enumeration,
    priority_contexts: &HashMap<Url, ContextKey>,
    project_contexts: &HashSet<ContextKey>,
    cancel: &AtomicBool,
) -> Result<HashSet<ContextKey>, String> {
    let mut context_keys = priority_contexts.values().cloned().collect::<HashSet<_>>();
    context_keys.extend(project_contexts.iter().cloned());
    for context_key in context_keys.clone() {
        if let Some(state) = loader.contexts.get(&context_key).cloned() {
            enumeration.retain_context(context_key, state);
        }
    }
    for index in 0..enumeration.paths.len() {
        if is_cancelled(cancel) {
            return Err(CANCELLATION_MESSAGE.to_string());
        }
        let path = enumeration.paths[index].path.clone();
        let owner = if let Some(owner) = enumeration.paths[index].owner.clone() {
            if let Some(state) = loader.contexts.get(&owner).cloned() {
                enumeration.retain_context(owner.clone(), state);
            } else {
                let state = input
                    .document_owners
                    .values()
                    .find(|known| known.key == owner)
                    .map(|known| known.state.clone())
                    .ok_or_else(|| {
                        format!("source {path:?} has an owner context that was not retained")
                    });
                let Ok(state) = state else {
                    enumeration.complete = false;
                    enumeration.reason.get_or_insert_with(|| {
                        format!("source {path:?} has an owner context that was not retained")
                    });
                    continue;
                };
                loader.contexts.insert(owner.clone(), state.clone());
                enumeration.retain_context(owner.clone(), state);
            }
            owner
        } else {
            let uri = Url::from_file_path(&path)
                .map_err(|()| format!("could not create a file URI for {path:?}"))?;
            let owner = snapshot_context_for_uri(loader, &uri, cancel)?;
            let Some(state) = loader.contexts.get(&owner).cloned() else {
                enumeration.complete = false;
                enumeration.reason.get_or_insert_with(|| {
                    format!("source {path:?} has an owner context that was not retained")
                });
                continue;
            };
            enumeration.retain_context(owner.clone(), state);
            owner
        };
        enumeration.assign_owner(&path, owner.clone());
        context_keys.insert(owner);
    }
    context_keys.extend(enumeration.contexts.keys().cloned());
    Ok(context_keys)
}

fn discover_project_metadata_contexts(
    loader: &mut Workspace,
    enumeration: &mut Enumeration,
    cancel: &AtomicBool,
) -> Result<HashSet<ContextKey>, String> {
    let mut descriptors = enumeration
        .baseline
        .paths
        .iter()
        .map(|baseline| baseline.path.clone())
        .filter(|path| {
            path.extension().is_some_and(|extension| {
                matches!(
                    extension.to_string_lossy().to_ascii_lowercase().as_str(),
                    "dproj" | "dpr" | "dpk"
                )
            })
        })
        .collect::<Vec<_>>();
    descriptors.sort_by(|left, right| left.to_string_lossy().cmp(&right.to_string_lossy()));
    descriptors.dedup_by(|left, right| path_key(left) == path_key(right));

    let roots = loader.workspace_root_paths();
    let mut options = loader.project_options();
    let mut contexts = HashSet::new();
    for descriptor in descriptors {
        if is_cancelled(cancel) {
            return Err(CANCELLATION_MESSAGE.to_string());
        }
        let uri = Url::from_file_path(&descriptor)
            .map_err(|()| format!("could not create a file URI for {descriptor:?}"))?;
        options.project_file = Some(descriptor.clone());
        let (key, context) =
            loader.readonly_context_for_uri(&uri, &descriptor, &roots, &options)?;
        for warning in context.warnings.iter().cloned() {
            loader.warn(warning);
        }
        loader.install_context(
            key.clone(),
            context,
            Vec::new(),
            HashMap::new(),
            &descriptor,
            Some(cancel),
        )?;
        let Some(state) = loader.contexts.get(&key).cloned() else {
            return Err(format!(
                "project context was not retained for {descriptor:?}"
            ));
        };
        enumeration.retain_context(key.clone(), state);
        contexts.insert(key);
    }
    Ok(contexts)
}

fn mapped_source_roots(context: &ProjectContext) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    let mapped_read_roots = context
        .overrides
        .read_roots()
        .into_iter()
        .map(|root| super::native_mapping_root(&root))
        .collect::<Vec<_>>();
    let mut add_root = |root: &Path| {
        let root = absolute_path(root.to_path_buf());
        if !roots
            .iter()
            .any(|existing: &PathBuf| path_key(existing) == path_key(&root))
        {
            roots.push(root);
        }
    };

    let mut add_entry = |entry: &pascal_project::ProjectPathEntry| match &entry.provenance {
        ProjectPathProvenance::Mapped { root } => add_root(root),
        ProjectPathProvenance::Configured => {
            if let Some(root) = mapped_read_roots
                .iter()
                .filter(|root| path_starts_with_native(&entry.path, root))
                .max_by_key(|root| root.components().count())
            {
                add_root(root);
            }
        }
        ProjectPathProvenance::LegacyNative => {}
    };
    for entry in &context.search_path_entries {
        add_entry(entry);
    }
    if let Some(entry) = &context.main_source_entry {
        add_entry(entry);
    }
    for entries in context.explicit_unit_entries.values() {
        for entry in entries {
            add_entry(entry);
        }
    }

    // Include paths intentionally retain their historical untagged API. Tie
    // them back to an effective mapping by destination containment so an
    // absolute mapped include can also contribute its consumer root.
    for include_path in &context.include_paths {
        for mapping in &context.overrides.path_mappings {
            let root = super::native_mapping_root(&mapping.to);
            if super::path_starts_with_native(include_path, &root) {
                add_root(&root);
            }
        }
    }
    // Package resolution can make a mapped destination relevant even when the
    // project has no unit-search, MainSource, reference, or include entry
    // under that destination. Keep the package root scoped to contexts that
    // actually request named packages; do not form a global mapping union.
    if !context.packages.is_empty() {
        for root in mapped_read_roots {
            add_root(&root);
        }
    }
    roots
}

fn enumerate_mapped_sources(
    workspace: &Workspace,
    context_states: &HashMap<ContextKey, ContextState>,
    contexts: &HashSet<ContextKey>,
    enumeration: &mut Enumeration,
    cancel: &AtomicBool,
) -> Result<(), String> {
    let mut context_keys = contexts.iter().cloned().collect::<Vec<_>>();
    context_keys.sort_by_key(|key| format!("{key:?}"));
    let entry_limit = MAX_RENAME_TRAVERSAL_ENTRIES;
    let mut stop = false;

    for context_key in context_keys {
        if is_cancelled(cancel) {
            return Err(CANCELLATION_MESSAGE.to_string());
        }
        let Some(context) = context_states
            .get(&context_key)
            .map(|state| state.context.clone())
        else {
            enumeration.complete = false;
            enumeration.reason.get_or_insert_with(|| {
                format!("mapped project context {context_key:?} was not retained")
            });
            continue;
        };
        let mapped_roots = mapped_source_roots(&context);
        if !context.discovery_complete
            && (!mapped_roots.is_empty() || context.override_error.is_some())
        {
            enumeration.complete = false;
            enumeration.reason.get_or_insert_with(|| {
                if mapped_roots.is_empty() {
                    "project context is ambiguous or incomplete".to_string()
                } else {
                    "mapped source roots belong to an ambiguous or incomplete project context"
                        .to_string()
                }
            });
            continue;
        }
        for root in mapped_roots {
            if is_cancelled(cancel) {
                return Err(CANCELLATION_MESSAGE.to_string());
            }
            let root = absolute_path(root);
            add_baseline_path(&mut enumeration.baseline, root.clone());
            let metadata = match fs::symlink_metadata(&root) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    enumeration.complete = false;
                    enumeration.reason.get_or_insert_with(|| {
                        format!(
                            "mapped source root {} is unavailable: {error}",
                            root.display()
                        )
                    });
                    continue;
                }
                Err(error) => {
                    enumeration.complete = false;
                    enumeration.reason.get_or_insert_with(|| {
                        format!(
                            "could not inspect mapped source root {}: {error}",
                            root.display()
                        )
                    });
                    continue;
                }
            };
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                enumeration.complete = false;
                enumeration.reason.get_or_insert_with(|| {
                    format!(
                        "mapped source root {} is not a regular directory",
                        root.display()
                    )
                });
                continue;
            }

            let walker = WalkDir::new(&root)
                .follow_links(false)
                .into_iter()
                .filter_entry(|entry| {
                    entry.depth() == 0
                        || !workspace.mapped_path_is_excluded(entry.path(), &root, &context_key)
                });
            for entry in walker {
                if is_cancelled(cancel) {
                    return Err(CANCELLATION_MESSAGE.to_string());
                }
                enumeration.visited_entries = enumeration.visited_entries.saturating_add(1);
                if enumeration.visited_entries > entry_limit {
                    enumeration.complete = false;
                    enumeration.reason.get_or_insert_with(|| {
                        format!("source traversal entry limit ({entry_limit}) reached")
                    });
                    stop = true;
                    break;
                }
                let entry = match entry {
                    Ok(entry) => entry,
                    Err(error) => {
                        enumeration.complete = false;
                        enumeration.reason.get_or_insert_with(|| {
                            format!(
                                "source traversal error under mapped root {}: {error}",
                                root.display()
                            )
                        });
                        continue;
                    }
                };
                let path = absolute_path(entry.path().to_path_buf());
                let file_type = entry.file_type();
                if file_type.is_symlink() {
                    continue;
                }
                if file_type.is_dir() {
                    add_baseline_path(&mut enumeration.baseline, path);
                    continue;
                }
                if file_type.is_file()
                    && is_pascal_path(&path)
                    && workspace.mapped_path_is_readable_under_root(&path, &root, &context_key)
                {
                    add_baseline_path(&mut enumeration.baseline, path.clone());
                    enumeration.add_path(path, Some(context_key.clone()));
                }
            }
            if stop {
                break;
            }
        }
        if stop {
            break;
        }
    }
    Ok(())
}

fn enumerate_external_overlays(
    workspace: &mut Workspace,
    input: &WorkspaceInput,
    active_contexts: &HashSet<ContextKey>,
    mode: SnapshotMode,
    enumeration: &mut Enumeration,
    cancel: &AtomicBool,
) -> Result<(), String> {
    for (overlay_uri, overlay) in &input.overlays {
        if is_cancelled(cancel) {
            return Err(CANCELLATION_MESSAGE.to_string());
        }
        let uri = canonical_file_uri(overlay_uri);
        if input.rejected_documents.contains(&uri) {
            continue;
        }
        let Ok(path) = uri.to_file_path() else {
            continue;
        };
        let path = absolute_path(path);
        if !is_pascal_path(&path) || workspace.accepts_path(&path) {
            continue;
        }
        let Some(owner) = input
            .document_owners
            .get(&uri)
            .or_else(|| input.document_owners.get(overlay_uri))
            .cloned()
        else {
            continue;
        };
        if !active_contexts
            .iter()
            .any(|active| overlay_contexts_match(active, &owner.key))
        {
            continue;
        }
        workspace
            .contexts
            .entry(owner.key.clone())
            .or_insert_with(|| owner.state.clone());
        let Some(state) = workspace.contexts.get(&owner.key) else {
            continue;
        };
        if context_incomplete_for_mode(mode, &state.context) {
            enumeration.complete = false;
            enumeration.reason.get_or_insert_with(|| {
                format!("project context is ambiguous or incomplete for {uri}")
            });
            continue;
        }
        if !workspace.ensure_supported_project_context(&path, &state.context, Some(&owner.key)) {
            continue;
        }
        if overlay.text.len() > input.options.limits.max_file_bytes {
            enumeration.complete = false;
            enumeration.reason.get_or_insert_with(|| {
                format!("open overlay {uri} exceeds the configured per-file limit")
            });
            continue;
        }
        add_baseline_path(&mut enumeration.baseline, path.clone());
        enumeration.add_path(path, Some(owner.key));
    }
    Ok(())
}

fn overlay_contexts_match(active: &ContextKey, owner: &ContextKey) -> bool {
    if active.project_file.is_some() && owner.project_file.is_some() {
        let mut active = active.clone();
        let mut owner = owner.clone();
        // An external mapped overlay has no workspace-root membership, while
        // its requesting source retains the workspace root. The project and
        // effective override identity still has to match exactly.
        active.workspace_root = None;
        owner.workspace_root = None;
        active == owner
    } else {
        active == owner
    }
}

fn add_priority_sources(
    workspace: &Workspace,
    input: &WorkspaceInput,
    priority: &[Url],
    priority_contexts: &HashMap<Url, ContextKey>,
    enumeration: &mut Enumeration,
    cancel: &AtomicBool,
) -> Result<(), String> {
    for uri in priority {
        if is_cancelled(cancel) {
            return Err(CANCELLATION_MESSAGE.to_string());
        }
        let Ok(path) = uri.to_file_path() else {
            continue;
        };
        let path = absolute_path(path);
        let Some(context_key) = priority_contexts.get(uri) else {
            continue;
        };
        let readable = workspace.contexts.get(context_key).is_some_and(|state| {
            let legacy_route = workspace.legacy_route_is_current(uri, &path, context_key);
            workspace.ensure_supported_project_context_with_legacy_route(
                &path,
                &state.context,
                Some(context_key),
                legacy_route,
            )
        });
        if !readable || !is_analyzable_source_path(&path) {
            continue;
        }
        if path.is_file() || input.overlays.contains_key(uri) {
            add_baseline_path(&mut enumeration.baseline, path.clone());
            enumeration.add_path(path, Some(context_key.clone()));
        }
    }
    Ok(())
}

pub(crate) fn build_snapshot(
    input: &WorkspaceInput,
    priority: &[Url],
    candidate_names: &[String],
    mode: SnapshotMode,
    priority_seed: Option<SnapshotSeed>,
    skip_imports_for: &[Url],
    cancel: &AtomicBool,
) -> Result<RenameSnapshot, String> {
    if is_cancelled(cancel) {
        return Err(CANCELLATION_MESSAGE.to_string());
    }

    let mut loader_options = input.options.clone();
    if mode == SnapshotMode::Workspace {
        loader_options.limits.max_files = loader_options
            .limits
            .max_files
            .saturating_add(MAX_SNAPSHOT_DEPENDENCY_FILES);
        loader_options.limits.max_total_bytes =
            loader_options.limits.max_total_bytes.saturating_add(
                loader_options
                    .limits
                    .max_file_bytes
                    .saturating_mul(MAX_SNAPSHOT_DEPENDENCY_FILES),
            );
    }
    let mut loader = Workspace::with_override_session(
        input.roots.clone(),
        loader_options,
        input.overrides.clone(),
    );
    loader.cached_documents = input.cached_documents.clone();
    loader.project_selections = input.project_selections.clone();
    loader.document_owners = input.document_owners.clone();
    for (uri, overlay) in &input.overlays {
        loader.open_documents.insert(
            uri.clone(),
            OpenDocument {
                text: Some(overlay.text.clone()),
                version: overlay.version,
                rejection: None,
                identity_generation: input.source_generation,
            },
        );
    }
    for uri in &input.rejected_documents {
        loader.open_documents.insert(
            uri.clone(),
            OpenDocument {
                text: None,
                version: 0,
                rejection: Some("open document was rejected by workspace limits".to_string()),
                identity_generation: input.source_generation,
            },
        );
    }

    let priority = priority.iter().map(canonical_file_uri).collect::<Vec<_>>();
    let mut priority_contexts = HashMap::new();
    for uri in &priority {
        let context_key = snapshot_context_for_uri(&mut loader, uri, cancel)?;
        priority_contexts.insert(uri.clone(), context_key);
    }
    #[cfg(test)]
    wait_at_snapshot_priority_barrier(&priority);
    if !input.project_selections.is_empty() {
        for uri in &priority {
            let context_key = priority_contexts
                .get(uri)
                .expect("priority context was captured");
            if loader
                .contexts
                .get(context_key)
                .is_some_and(|state| has_invalid_project_selection(&state.context))
            {
                return Err(format!(
                    "project selection is invalid; select a current project or Automatic for {uri}"
                ));
            }
        }
    }
    if mode == SnapshotMode::Workspace {
        for uri in &priority {
            if is_cancelled(cancel) {
                return Err(CANCELLATION_MESSAGE.to_string());
            }
            let context_key = priority_contexts
                .get(uri)
                .expect("priority context was captured");
            if !loader
                .contexts
                .get(context_key)
                .is_some_and(|state| state.context.discovery_complete)
            {
                let target_has_include = priority_seed
                    .as_ref()
                    .is_some_and(|seed| may_contain_include_directive(seed.record.text.as_bytes()));
                if target_has_include {
                    return Err(format!(
                        "rename workspace scan incomplete: include dependency context is ambiguous or incomplete for {uri}"
                    ));
                }
                return Err(format!(
                    "rename workspace scan incomplete: project context is ambiguous or incomplete for {uri}"
                ));
            }
        }
    }
    let mut enumeration = enumerate_sources(
        &mut loader,
        input,
        &priority,
        &priority_contexts,
        candidate_names,
        mode,
        cancel,
    )?;
    let project_contexts = if mode == SnapshotMode::WorkspaceSymbols {
        discover_project_metadata_contexts(&mut loader, &mut enumeration, cancel)?
    } else {
        HashSet::new()
    };
    let enumerated_contexts =
        if mode == SnapshotMode::Workspace || mode == SnapshotMode::WorkspaceSymbols {
            discover_enumerated_contexts(
                &mut loader,
                input,
                &mut enumeration,
                &priority_contexts,
                &project_contexts,
                cancel,
            )?
        } else {
            priority_contexts.values().cloned().collect()
        };
    for (context_key, state) in &enumeration.contexts {
        loader
            .contexts
            .entry(context_key.clone())
            .or_insert_with(|| state.clone());
    }
    if mode == SnapshotMode::Workspace || mode == SnapshotMode::WorkspaceSymbols {
        let mapped_contexts = if mode == SnapshotMode::WorkspaceSymbols {
            enumerated_contexts.clone()
        } else {
            priority_contexts.values().cloned().collect()
        };
        let context_states = enumeration.contexts.clone();
        enumerate_mapped_sources(
            &loader,
            &context_states,
            &mapped_contexts,
            &mut enumeration,
            cancel,
        )?;
        enumerate_external_overlays(
            &mut loader,
            input,
            &mapped_contexts,
            mode,
            &mut enumeration,
            cancel,
        )?;
    }
    add_priority_sources(
        &loader,
        input,
        &priority,
        &priority_contexts,
        &mut enumeration,
        cancel,
    )?;
    enumeration.sort_paths(&priority);
    let auto_import_complete = enumeration.auto_import_complete;
    let paths = enumeration.paths;
    let mut auto_import_unit_providers = enumeration.auto_import_unit_providers;
    let mut complete = enumeration.complete;
    let mut incomplete_reason = enumeration.reason;
    let mut baseline = enumeration.baseline;
    let mut baseline_content_hashes = enumeration.baseline_content_hashes;
    let mut baseline_contents = enumeration.baseline_contents;
    capture_consumed_configuration_baseline(
        priority_seed
            .as_ref()
            .map_or(&[][..], |seed| seed.consumed_configuration.as_slice()),
        &mut baseline,
        &mut baseline_content_hashes,
        &mut baseline_contents,
        cancel,
    )?;
    for rejected_uri in &input.rejected_documents {
        if matches!(
            mode,
            SnapshotMode::Local | SnapshotMode::LocalWithImports | SnapshotMode::Assistance
        ) && !priority
            .iter()
            .any(|priority_uri| priority_uri == rejected_uri)
        {
            continue;
        }
        let Ok(path) = rejected_uri.to_file_path() else {
            continue;
        };
        let path = absolute_path(path);
        if loader.accepts_path(&path) && is_pascal_path(&path) {
            complete = false;
            incomplete_reason.get_or_insert_with(|| {
                format!(
                    "open document {rejected_uri} was rejected and cannot be used for rename analysis"
                )
            });
        }
    }
    let mut sources = HashMap::new();
    let mut records = HashMap::new();
    let mut readable = HashSet::new();
    let mut editable = HashSet::new();
    let mut contexts = HashMap::new();
    let mut index = NavigationIndex::new();
    index.set_auto_import_discovery_complete(auto_import_complete);
    let mut indexed_sizes = HashMap::new();
    let mut indexed_stamps = HashMap::new();
    let mut indexed_uris = HashSet::new();
    let mut retained_files = 0usize;
    let mut retained_bytes = 0usize;
    let mut scanned_bytes = 0usize;
    let mut auto_import_provider_observations = Vec::new();
    let mut auto_import_observation_paths = HashSet::new();
    let allow_incomplete_context_for: &[Url] = if mode == SnapshotMode::Local {
        skip_imports_for
    } else {
        &[]
    };
    let mut include_errors = Vec::new();
    let candidate_names_are_ascii = candidate_names
        .iter()
        .all(|name| name.trim_start_matches('&').is_ascii());

    for uri in &priority {
        if mode == SnapshotMode::Local {
            break;
        }
        if is_cancelled(cancel) {
            return Err(CANCELLATION_MESSAGE.to_string());
        }
        let context_key = priority_contexts
            .get(uri)
            .expect("priority context was captured")
            .clone();
        capture_context_baseline(
            &loader,
            &context_key,
            &mut baseline,
            &mut baseline_content_hashes,
            &mut baseline_contents,
            mode == SnapshotMode::WorkspaceSymbols,
            cancel,
        )?;
        contexts.insert(uri.clone(), context_key.clone());
        if !loader
            .contexts
            .get(&context_key)
            .is_some_and(|state| state.context.discovery_complete)
        {
            complete = false;
            incomplete_reason.get_or_insert_with(|| {
                format!("project context is ambiguous or incomplete for {uri}")
            });
        }
    }
    for enumerated in paths {
        if is_cancelled(cancel) {
            return Err(CANCELLATION_MESSAGE.to_string());
        }
        let path = enumerated.path;
        let enumerated_owner = enumerated.owner;
        let uri = match Url::from_file_path(&path) {
            Ok(uri) => uri,
            Err(()) => continue,
        };
        if input.rejected_documents.contains(&uri) {
            complete = false;
            incomplete_reason.get_or_insert_with(|| {
                format!("open document {uri} was rejected and cannot be used for rename analysis")
            });
            continue;
        }
        let is_priority = priority.iter().any(|priority_uri| priority_uri == &uri);
        let source_context_key = if let Some(context_key) = enumerated_owner.clone() {
            context_key
        } else if let Some(context_key) = priority_contexts.get(&uri) {
            context_key.clone()
        } else {
            snapshot_context_for_uri(&mut loader, &uri, cancel)?
        };
        let (read_policy, path_entry) =
            match snapshot_payload_dependency(&loader, &source_context_key, &path) {
                Ok(dependency) => dependency,
                Err(error) if is_priority => return Err(error),
                Err(error) => {
                    complete = false;
                    incomplete_reason.get_or_insert(error);
                    continue;
                }
            };
        let mut auto_import_provider_recorded = false;
        let (source, record, source_bytes) = if let Some(seed) =
            priority_seed.as_ref().filter(|seed| seed.record.uri == uri)
        {
            let source_bytes = seed
                .record
                .stamp
                .as_ref()
                .map(|stamp| stamp.bytes as usize)
                .unwrap_or_else(|| seed.record.text.len());
            (seed.record.text.clone(), seed.record.clone(), source_bytes)
        } else if let Some(overlay) = input.overlays.get(&uri) {
            (
                overlay.text.clone(),
                SourceRecord {
                    uri: uri.clone(),
                    text: overlay.text.clone(),
                    version: Some(overlay.version),
                    stamp: None,
                    open: true,
                    path: None,
                    path_stamp: None,
                    content_hash: None,
                    parsed_text_hash: Some(text_content_hash(&overlay.text)),
                    content_bytes: None,
                    candidate_membership: None,
                    candidate_observations: Vec::new(),
                    read_policy: Some(read_policy.clone()),
                    path_entry: Some(path_entry.clone()),
                    include_payload: false,
                    missing_provider_candidate: false,
                    directory_observation: false,
                    missing_provider_scope: None,
                    auto_import_provider_observation: false,
                    auto_import_scopes: Vec::new(),
                },
                overlay.text.len(),
            )
        } else {
            let scan = match read_scan_source(&path, &read_policy, &path_entry, cancel) {
                Ok(scan) => scan,
                Err(error) if error == CANCELLATION_MESSAGE => return Err(error),
                Err(error) => {
                    if is_priority {
                        return Err(format!(
                            "rename workspace scan could not read {path:?}: {error}"
                        ));
                    }
                    complete = false;
                    incomplete_reason.get_or_insert_with(|| {
                        format!("rename workspace scan could not read {path:?}: {error}")
                    });
                    continue;
                }
            };
            scanned_bytes = scanned_bytes.saturating_add(scan.bytes);
            if mode == SnapshotMode::Assistance {
                auto_import_provider_observations.push(AutoImportProviderObservation {
                    path: path.clone(),
                    content_hash: scan.content_hash,
                    read_policy: read_policy.clone(),
                    path_entry: path_entry.clone(),
                });
            } else {
                baseline_content_hashes
                    .entry(path_key(&path))
                    .or_insert(scan.content_hash);
                baseline.set_payload_dependency(&path, read_policy.clone(), path_entry.clone());
            }
            if scanned_bytes > MAX_RENAME_SCANNED_BYTES {
                complete = false;
                incomplete_reason.get_or_insert_with(|| {
                    format!("rename source scan byte limit ({MAX_RENAME_SCANNED_BYTES}) reached")
                });
                break;
            }
            if is_cancelled(cancel) {
                return Err(CANCELLATION_MESSAGE.to_string());
            }
            let source = decode_bytes(&scan.data).into_owned();
            if mode == SnapshotMode::Assistance && !is_priority {
                auto_import_provider_recorded = true;
                if let Some(unit_name) = source_unit_name(&source, cancel)? {
                    auto_import_unit_providers
                        .entry(unit_name)
                        .or_default()
                        .push(uri.clone());
                }
            }
            let direct_candidate = candidate_names.is_empty()
                || !candidate_names_are_ascii
                || contains_any_identifier_bytes(&scan.data, candidate_names)
                || (mode == SnapshotMode::Assistance
                    && contains_any_identifier_prefix_bytes(&scan.data, candidate_names));
            let may_contain_include = may_contain_include_directive(&scan.data);
            if !is_priority && !direct_candidate && !may_contain_include {
                continue;
            }
            let source = shared_resolver::decode_source_bytes(&scan.data);
            let stamp = disk_stamp(&path)
                .ok_or_else(|| format!("rename workspace scan could not stat source {path:?}"))?;
            (
                source.clone(),
                SourceRecord {
                    uri: uri.clone(),
                    text: source.clone(),
                    version: None,
                    stamp: Some(stamp),
                    open: false,
                    path: None,
                    path_stamp: None,
                    content_hash: Some(scan.content_hash),
                    parsed_text_hash: Some(text_content_hash(&source)),
                    content_bytes: None,
                    candidate_membership: None,
                    candidate_observations: Vec::new(),
                    read_policy: Some(read_policy.clone()),
                    path_entry: Some(path_entry.clone()),
                    include_payload: false,
                    missing_provider_candidate: false,
                    directory_observation: false,
                    missing_provider_scope: None,
                    auto_import_provider_observation: false,
                    auto_import_scopes: Vec::new(),
                },
                scan.bytes,
            )
        };
        if mode == SnapshotMode::Assistance && !is_priority && !auto_import_provider_recorded {
            if let Some(unit_name) = source_unit_name(&source, cancel)? {
                auto_import_unit_providers
                    .entry(unit_name)
                    .or_default()
                    .push(uri.clone());
            }
        }
        if !record.open {
            baseline.set_payload_dependency(&path, read_policy.clone(), path_entry.clone());
        }
        if let Some(content_hash) = record.content_hash {
            baseline_content_hashes
                .entry(path_key(&path))
                .or_insert(content_hash);
        }
        let may_contain_include = may_contain_include_directive(source.as_bytes());
        let mut indexed_source = source.clone();
        if mode != SnapshotMode::WorkspaceSymbols && may_contain_include {
            let expansion_context = loader
                .contexts
                .get(&source_context_key)
                .map(|state| state.context.effective_conditional_context())
                .unwrap_or_default();
            match loader.expand_source_with_cancel(&uri, &source, &source_context_key, Some(cancel))
            {
                Ok(expansion) => {
                    let mut expansion = expansion;
                    let conditional = conditional::analyze_with_context_and_cancel(
                        expansion.expanded.text(),
                        &expansion_context,
                        cancel,
                    );
                    include_expansion::reconcile_conditional_completeness(
                        &mut expansion,
                        &conditional,
                    );
                    if !expansion.complete && mode != SnapshotMode::Assistance {
                        complete = false;
                        if let Some(error) = expansion.errors.first() {
                            incomplete_reason.get_or_insert_with(|| {
                                format!("include expansion incomplete: {error}")
                            });
                        } else {
                            incomplete_reason.get_or_insert_with(|| {
                                "include expansion did not complete".to_string()
                            });
                        }
                    }
                    // Keep the raw expanded buffer here.  NavigationIndex
                    // owns conditional projection and records the resulting
                    // unknown/inactive spans; passing an already-projected
                    // buffer would erase the information needed to fail
                    // closed for conditional references and edits.
                    indexed_source = expansion.expanded.text().to_owned();
                    loader.store_expansion(&uri, &source_context_key, source.clone(), expansion);
                    let expanded_source_contains_candidate = candidate_names.is_empty()
                        || contains_any_identifier(&indexed_source, candidate_names)
                        || (mode == SnapshotMode::Assistance
                            && contains_any_identifier_prefix(&indexed_source, candidate_names));
                    if expanded_source_contains_candidate {
                        if let Some(expansion) = loader.expansions.get(&uri).cloned() {
                            retain_expansion_dependencies(
                                &mut loader,
                                input,
                                &uri,
                                &source_context_key,
                                &expansion,
                                &mut sources,
                                &mut records,
                                &mut readable,
                                &mut editable,
                                &mut contexts,
                                &mut baseline,
                                &mut baseline_content_hashes,
                                &mut retained_files,
                                &mut retained_bytes,
                                &mut complete,
                                &mut incomplete_reason,
                                cancel,
                            )?;
                        }
                    }
                }
                Err(error) if error == CANCELLATION_MESSAGE => return Err(error),
                Err(error) => {
                    complete = false;
                    incomplete_reason
                        .get_or_insert_with(|| format!("include expansion failed: {error}"));
                }
            }
        }
        let should_index = is_priority
            || candidate_names.is_empty()
            || contains_any_identifier(&indexed_source, candidate_names)
            || (mode == SnapshotMode::Assistance
                && contains_any_identifier_prefix(&indexed_source, candidate_names));
        if !should_index {
            let owner_directives = directives(&source);
            if owner_directives
                .iter()
                .any(|directive| directive.kind == DirectiveKind::Include)
            {
                let Some(summary) = include_owner_summary(&source, &owner_directives) else {
                    complete = false;
                    incomplete_reason.get_or_insert_with(|| {
                        format!(
                            "include owner {path:?} exceeds the bounded directive summary limit ({MAX_RENAME_INCLUDE_OWNER_SUMMARY_BYTES} bytes)"
                        )
                    });
                    continue;
                };
                let summary_bytes = summary.len();
                if summary_bytes > input.options.limits.max_file_bytes {
                    complete = false;
                    incomplete_reason.get_or_insert_with(|| {
                        format!(
                            "include owner {path:?} directive summary exceeds the configured per-file limit"
                        )
                    });
                    continue;
                }
                if retained_files >= input.options.limits.max_files
                    || retained_bytes.saturating_add(summary_bytes)
                        > input.options.limits.max_total_bytes
                {
                    complete = false;
                    incomplete_reason.get_or_insert_with(|| {
                        if retained_files >= input.options.limits.max_files {
                            format!(
                                "retained source file limit ({}) reached while retaining include owners",
                                input.options.limits.max_files
                            )
                        } else {
                            format!(
                                "retained source byte limit ({}) reached while retaining include owners",
                                input.options.limits.max_total_bytes
                            )
                        }
                    });
                    continue;
                }

                if !contexts.contains_key(&uri) {
                    let context_key = source_context_key.clone();
                    capture_context_baseline(
                        &loader,
                        &context_key,
                        &mut baseline,
                        &mut baseline_content_hashes,
                        &mut baseline_contents,
                        mode == SnapshotMode::WorkspaceSymbols,
                        cancel,
                    )?;
                    contexts.insert(uri.clone(), context_key.clone());
                    if loader
                        .contexts
                        .get(&context_key)
                        .is_some_and(|state| context_incomplete_for_mode(mode, &state.context))
                    {
                        complete = false;
                        incomplete_reason.get_or_insert_with(|| {
                            format!("project context is ambiguous or incomplete for {uri}")
                        });
                    }
                }

                let mut summary_record = record;
                summary_record.text = summary.clone();
                if summary_record.open {
                    summary_record.content_hash = Some(text_content_hash(&source));
                } else {
                    let stamp = summary_record.stamp.take();
                    summary_record.path = Some(path.clone());
                    summary_record.path_stamp = stamp.and_then(|_| path_stamp(&path));
                }
                sources.insert(uri.clone(), summary);
                records.insert(uri, summary_record);
                retained_files = retained_files.saturating_add(1);
                retained_bytes = retained_bytes.saturating_add(summary_bytes);
            }
            continue;
        }
        if source.len() > input.options.limits.max_file_bytes {
            complete = false;
            incomplete_reason.get_or_insert_with(|| {
                format!(
                    "source {} exceeds the configured per-file limit",
                    path.display()
                )
            });
            continue;
        }
        if retained_files >= input.options.limits.max_files
            || retained_bytes.saturating_add(source_bytes) > input.options.limits.max_total_bytes
        {
            complete = false;
            incomplete_reason.get_or_insert_with(|| {
                if retained_files >= input.options.limits.max_files {
                    format!(
                        "retained source file limit ({}) reached while analyzing rename candidates",
                        input.options.limits.max_files
                    )
                } else {
                    format!(
                        "retained source byte limit ({}) reached while analyzing rename candidates",
                        input.options.limits.max_total_bytes
                    )
                }
            });
            continue;
        }

        if !contexts.contains_key(&uri) {
            let context_key = source_context_key.clone();
            capture_context_baseline(
                &loader,
                &context_key,
                &mut baseline,
                &mut baseline_content_hashes,
                &mut baseline_contents,
                mode == SnapshotMode::WorkspaceSymbols,
                cancel,
            )?;
            contexts.insert(uri.clone(), context_key.clone());
            if loader
                .contexts
                .get(&context_key)
                .is_some_and(|state| context_incomplete_for_mode(mode, &state.context))
            {
                complete = false;
                incomplete_reason.get_or_insert_with(|| {
                    format!("project context is ambiguous or incomplete for {uri}")
                });
            }
        }
        let conditional_context = contexts
            .get(&uri)
            .and_then(|context_key| loader.contexts.get(context_key))
            .map(|state| {
                if allow_incomplete_context_for
                    .iter()
                    .any(|allowed_uri| allowed_uri == &uri)
                    && !state.context.discovery_complete
                {
                    ConditionalContext::default()
                } else {
                    state.context.effective_conditional_context()
                }
            })
            .unwrap_or_default();
        let cached = input
            .cached_documents
            .get(&uri)
            .filter(|cached| {
                loader
                    .contexts
                    .get(&source_context_key)
                    .is_some_and(|state| state.context == cached.context)
            })
            .map(|cached| cached.parsed.clone());
        index
            .update_with_context_and_cached_with_cancel(
                uri.clone(),
                indexed_source.clone(),
                &conditional_context,
                cached,
                cancel,
            )
            .map_err(|error| format!("rename workspace scan could not index {uri}: {error}"))?;
        retained_files = retained_files.saturating_add(1);
        retained_bytes = retained_bytes.saturating_add(source_bytes);
        indexed_uris.insert(uri.clone());
        indexed_sizes.insert(uri.clone(), indexed_source.len());
        if let Some(stamp) = record.stamp.clone() {
            indexed_stamps.insert(uri.clone(), stamp);
        }
        sources.insert(uri.clone(), source);
        if is_readable_source_for_context(&loader, &path, contexts.get(&uri)) {
            readable.insert(uri.clone());
        }
        if is_editable_source_path(&loader, &path) {
            editable.insert(uri.clone());
        }
        records.insert(uri, record);
    }

    if sources.is_empty() && mode != SnapshotMode::WorkspaceSymbols {
        return Err(
            "rename workspace source scan was empty; no Pascal sources were retained".to_string(),
        );
    }
    for providers in auto_import_unit_providers.values_mut() {
        providers.sort_by(|left, right| left.as_str().cmp(right.as_str()));
        providers.dedup();
    }
    for (uri, context_key) in &contexts {
        loader
            .document_contexts
            .insert(uri.clone(), context_key.clone());
    }
    index.set_auto_import_unit_providers(auto_import_unit_providers.clone());
    loader.index = index;
    loader.indexed_files = indexed_uris.clone();
    loader.indexed_sizes = indexed_sizes;
    loader.indexed_bytes = loader.indexed_sizes.values().sum();
    loader.disk_stamps = indexed_stamps;

    let mut uris: Vec<Url> = indexed_uris.iter().cloned().collect();
    uris.sort_by(|left, right| left.as_str().cmp(right.as_str()));
    let mut pins: HashSet<Url> = indexed_uris;
    if mode != SnapshotMode::WorkspaceSymbols {
        for uri in uris {
            if is_cancelled(cancel) {
                return Err(CANCELLATION_MESSAGE.to_string());
            }
            if let Some(context_key) = contexts.get(&uri).cloned() {
                if skip_imports_for.iter().any(|skip_uri| skip_uri == &uri) {
                    loader
                        .index
                        .bind_imports(&uri, std::iter::empty::<(String, Url)>());
                    continue;
                }
                // Dependency loading intentionally deduplicates physical
                // providers.  Completeness must instead be established at
                // every parsed import site so project/namespace aliases that
                // bind to one provider do not look incomplete merely because
                // their dependency URI is shared.
                let imports = loader.index.imports(&uri);
                loader.load_imports_with_cancel(&uri, &context_key, &mut pins, Some(cancel))?;
                if imports.iter().any(|import| {
                    loader
                        .index
                        .import_provider_uri(&uri, &import.name)
                        .is_none()
                }) {
                    complete = false;
                    incomplete_reason.get_or_insert_with(|| {
                        format!("one or more imports could not be resolved for {uri}")
                    });
                }
            } else {
                loader
                    .index
                    .bind_imports(&uri, std::iter::empty::<(String, Url)>());
            }
        }
    }

    if mode != SnapshotMode::WorkspaceSymbols {
        if let Some(evicted_uri) = pins.iter().find(|uri| !loader.indexed_files.contains(*uri)) {
            complete = false;
            incomplete_reason.get_or_insert_with(|| {
                format!(
                    "retained rename source was evicted before binding completed: {evicted_uri}"
                )
            });
        }
    }
    if mode == SnapshotMode::Assistance {
        if let Some(current_uri) = priority.first() {
            let Some(context_key) = priority_contexts.get(current_uri).cloned() else {
                auto_import_unit_providers.clear();
                loader
                    .index
                    .set_auto_import_unit_providers(auto_import_unit_providers.clone());
                return Err(format!(
                    "auto-import provider context was not retained for {current_uri}"
                ));
            };
            retain_resolvable_auto_import_providers(
                &mut loader,
                current_uri,
                &context_key,
                &mut auto_import_unit_providers,
                &mut pins,
                candidate_names,
                cancel,
            )?;
            loader
                .index
                .set_auto_import_unit_providers(auto_import_unit_providers.clone());
        }
    }
    if mode == SnapshotMode::Assistance {
        if let Some(current_uri) = priority.first() {
            if let Some(record) = records.get_mut(current_uri) {
                record.auto_import_scopes = assistance_auto_import_scopes(
                    &loader,
                    &priority_contexts,
                    &auto_import_unit_providers,
                    candidate_names,
                );
            }
        }
    }
    let auto_import_context = if mode == SnapshotMode::Assistance {
        match (
            priority.first(),
            priority_seed
                .as_ref()
                .and_then(|seed| seed.completion_position),
        ) {
            (Some(current_uri), Some(position)) => loader
                .index
                .completion_context_may_auto_import(current_uri, position, cancel)?,
            _ => false,
        }
    } else {
        false
    };
    let retain_auto_import_observations = auto_import_context
        && priority.first().is_some_and(|current_uri| {
            records
                .get(current_uri)
                .is_some_and(|record| !record.auto_import_scopes.is_empty())
        });
    if retain_auto_import_observations {
        for observation in auto_import_provider_observations {
            let key = path_key(&observation.path);
            baseline_content_hashes
                .entry(key.clone())
                .or_insert(observation.content_hash);
            baseline.set_payload_dependency(
                &observation.path,
                observation.read_policy,
                observation.path_entry,
            );
            auto_import_observation_paths.insert(key);
        }
    }
    if mode == SnapshotMode::Assistance && !complete {
        loader.index.set_auto_import_discovery_complete(false);
    }

    // A source-bearing include is represented by each owning virtual root.
    // Keeping the same physical fragment as an additional standalone parsed
    // document makes strict rename/reference scans treat root-visible names as
    // unresolved in the fragment's isolated scope.  Retain its physical text
    // and records for mapping/revalidation, but remove the standalone parser
    // document whenever an owning expansion proved the dependency.
    let included_uris = loader.include_parents.keys().cloned().collect::<Vec<_>>();
    for included_uri in included_uris {
        loader.index.remove(&included_uri);
    }
    let indexed_uris = loader.indexed_files.clone();
    for uri in indexed_uris {
        if records.contains_key(&uri) {
            continue;
        }
        let path = uri
            .to_file_path()
            .map(absolute_path)
            .map_err(|_| format!("loaded dependency is not a file URI: {uri}"))?;
        let (source, mut record) = if let Some(overlay) = input.overlays.get(&uri) {
            (
                overlay.text.clone(),
                SourceRecord {
                    uri: uri.clone(),
                    text: overlay.text.clone(),
                    version: Some(overlay.version),
                    stamp: None,
                    open: true,
                    path: None,
                    path_stamp: None,
                    content_hash: None,
                    parsed_text_hash: Some(text_content_hash(&overlay.text)),
                    content_bytes: None,
                    candidate_membership: None,
                    candidate_observations: Vec::new(),
                    read_policy: None,
                    path_entry: None,
                    include_payload: false,
                    missing_provider_candidate: false,
                    directory_observation: false,
                    missing_provider_scope: None,
                    auto_import_provider_observation: false,
                    auto_import_scopes: Vec::new(),
                },
            )
        } else {
            let source = loader
                .index
                .source_text(&uri)
                .ok_or_else(|| format!("loaded dependency {path:?} lost its indexed source"))?
                .to_owned();
            let stamp = loader
                .disk_stamps
                .get(&uri)
                .cloned()
                .ok_or_else(|| format!("loaded dependency {path:?} lost its disk identity"))?;
            baseline.add_path(path.clone(), path_stamp(&path));
            (
                source.clone(),
                SourceRecord {
                    uri: uri.clone(),
                    text: source.clone(),
                    version: None,
                    stamp: Some(stamp),
                    open: false,
                    path: None,
                    path_stamp: None,
                    content_hash: None,
                    parsed_text_hash: Some(text_content_hash(&source)),
                    content_bytes: None,
                    candidate_membership: None,
                    candidate_observations: Vec::new(),
                    read_policy: None,
                    path_entry: None,
                    include_payload: false,
                    missing_provider_candidate: false,
                    directory_observation: false,
                    missing_provider_scope: None,
                    auto_import_provider_observation: false,
                    auto_import_scopes: Vec::new(),
                },
            )
        };
        if !record.open {
            let context_key = loader.document_contexts.get(&uri).ok_or_else(|| {
                format!("loaded dependency {path:?} has no retained project context")
            })?;
            let (read_policy, path_entry) =
                snapshot_payload_dependency(&loader, context_key, &path)?;
            add_baseline_content_hash(
                &path,
                &mut baseline,
                &mut baseline_content_hashes,
                &read_policy,
                &path_entry,
                cancel,
                true,
            )?;
            record.content_hash = baseline_content_hashes.get(&path_key(&path)).copied();
            record.read_policy = Some(read_policy);
            record.path_entry = Some(path_entry);
        }
        if let Some(context_key) = loader.document_contexts.get(&uri).cloned() {
            contexts.insert(uri.clone(), context_key);
        }
        if is_readable_source_for_context(&loader, &path, loader.document_contexts.get(&uri)) {
            readable.insert(uri.clone());
        }
        if is_editable_source_path(&loader, &path) {
            editable.insert(uri.clone());
        }
        sources.insert(uri.clone(), source);
        records.insert(uri, record);
    }

    if mode != SnapshotMode::WorkspaceSymbols {
        let include_audit = audit_includes(IncludeAuditor {
            input,
            sources: &sources,
            loader: &mut loader,
            contexts: &mut contexts,
            baseline: &mut baseline,
            baseline_content_hashes: &mut baseline_content_hashes,
            candidate_names,
            max_file_bytes: input.options.limits.max_file_bytes,
            observe_directory_stamps: mode == SnapshotMode::WorkspaceSymbols,
            allow_incomplete_context_for,
            cancel,
            cache: HashMap::new(),
            resolution_cache: HashMap::new(),
            resolvers: HashMap::new(),
            active: HashSet::new(),
            name_free_assistance: mode == SnapshotMode::Assistance,
            baseline_contents: &mut baseline_contents,
            files_read: 0,
            bytes_read: 0,
            directives_seen: 0,
            stopped: false,
            result: IncludeAuditResult::default(),
        })?;
        include_errors.extend(include_audit.errors);
        if let Some(reason) = include_audit.incomplete_reason {
            complete = false;
            incomplete_reason.get_or_insert(reason);
        }
    }
    if let Some(analysis_records) = loader.analysis_records.take() {
        for (uri, record) in analysis_records {
            if record.include_payload
                || record.open && record.version.is_some()
                || record.content_hash.is_some()
                    && record.read_policy.is_some()
                    && record.path_entry.is_some()
            {
                super::resolver::merge_source_record(&mut records, SourceRecord { uri, ..record });
            }
        }
    }

    for (context_key, state) in std::mem::take(&mut enumeration.contexts) {
        if let Some(live) = loader.contexts.get_mut(&context_key) {
            live.merge_observations(&state);
        } else {
            loader.contexts.insert(context_key, state);
        }
    }
    for context_key in loader.contexts.keys().cloned().collect::<Vec<_>>() {
        capture_context_baseline(
            &loader,
            &context_key,
            &mut baseline,
            &mut baseline_content_hashes,
            &mut baseline_contents,
            mode == SnapshotMode::WorkspaceSymbols,
            cancel,
        )?;
    }
    let mut baseline_records = baseline
        .paths
        .into_iter()
        .filter_map(|baseline| {
            let content_hash = baseline_content_hashes
                .get(&path_key(&baseline.path))
                .copied();
            path_record_at(
                baseline.path.clone(),
                baseline.stamp,
                content_hash,
                baseline_contents.get(&path_key(&baseline.path)).cloned(),
                baseline.candidate_membership,
                baseline.read_policy,
                baseline.path_entry,
                baseline.include_payload,
            )
        })
        .collect::<Vec<_>>();
    for record in &mut baseline_records {
        record.auto_import_provider_observation = record
            .path
            .as_ref()
            .is_some_and(|path| auto_import_observation_paths.contains(&path_key(path)));
    }
    Ok(RenameSnapshot {
        index: loader.index,
        sources,
        records,
        expansions: loader.expansions,
        readable,
        editable,
        complete,
        incomplete_reason,
        include_errors,
        baseline_records,
        mode,
    })
}

#[allow(clippy::too_many_arguments)]
fn retain_expansion_dependencies(
    loader: &mut Workspace,
    input: &WorkspaceInput,
    root_uri: &Url,
    context_key: &ContextKey,
    expansion: &super::ExpansionRecord,
    sources: &mut HashMap<Url, String>,
    records: &mut HashMap<Url, SourceRecord>,
    readable: &mut HashSet<Url>,
    editable: &mut HashSet<Url>,
    contexts: &mut HashMap<Url, ContextKey>,
    baseline: &mut BaselineAccumulator,
    baseline_content_hashes: &mut HashMap<String, u64>,
    retained_files: &mut usize,
    retained_bytes: &mut usize,
    complete: &mut bool,
    incomplete_reason: &mut Option<String>,
    cancel: &AtomicBool,
) -> Result<(), String> {
    let dependency_entries = expansion.dependency_entries.clone();
    let mut dependencies = expansion
        .source_texts
        .iter()
        .filter(|(uri, _)| *uri != root_uri)
        .map(|(uri, source)| (uri.clone(), source.clone()))
        .collect::<Vec<_>>();
    dependencies.sort_by(|left, right| left.0.as_str().cmp(right.0.as_str()));

    for (uri, source) in dependencies {
        if is_cancelled(cancel) {
            return Err(CANCELLATION_MESSAGE.to_string());
        }
        if records.contains_key(&uri) {
            if sources
                .get(&uri)
                .is_some_and(|existing| existing != &source)
            {
                *complete = false;
                incomplete_reason.get_or_insert_with(|| {
                    format!("include source {uri} was observed under conflicting contexts")
                });
            }
            continue;
        }
        let path = uri
            .to_file_path()
            .map(absolute_path)
            .map_err(|_| format!("include source is not a file URI: {uri}"))?;
        let (read_policy, path_entry) = if let Some(path_entry) = dependency_entries.get(&uri) {
            let Some(context) = loader.contexts.get(context_key).map(|state| &state.context) else {
                *complete = false;
                incomplete_reason.get_or_insert_with(|| {
                    format!("include source {uri} has no retained project context")
                });
                continue;
            };
            (context.read_policy.clone(), path_entry.clone())
        } else {
            match snapshot_payload_dependency(loader, context_key, &path) {
                Ok(value) => value,
                Err(error) if loader.accepts_path(&path) => {
                    let Some(context) =
                        loader.contexts.get(context_key).map(|state| &state.context)
                    else {
                        *complete = false;
                        incomplete_reason.get_or_insert(error);
                        continue;
                    };
                    (
                        context.read_policy.clone(),
                        ProjectPathEntry {
                            path: path.clone(),
                            provenance: ProjectPathProvenance::LegacyNative,
                        },
                    )
                }
                Err(error) => {
                    *complete = false;
                    incomplete_reason.get_or_insert(error);
                    continue;
                }
            }
        };
        contexts.insert(uri.clone(), context_key.clone());
        loader
            .document_contexts
            .insert(uri.clone(), context_key.clone());

        let record = if let Some(document) = loader.open_documents.get(&uri) {
            let Some(open_source) = document.text.as_ref() else {
                *complete = false;
                incomplete_reason.get_or_insert_with(|| {
                    format!("include source {uri} was rejected and cannot be retained")
                });
                continue;
            };
            if open_source != &source {
                *complete = false;
                incomplete_reason.get_or_insert_with(|| {
                    format!("include source {uri} changed during expansion")
                });
                continue;
            }
            SourceRecord {
                uri: uri.clone(),
                text: source.clone(),
                version: Some(document.version),
                stamp: None,
                open: true,
                path: None,
                path_stamp: None,
                content_hash: None,
                parsed_text_hash: Some(text_content_hash(&source)),
                content_bytes: None,
                candidate_membership: None,
                candidate_observations: Vec::new(),
                read_policy: Some(read_policy.clone()),
                path_entry: Some(path_entry.clone()),
                include_payload: false,
                missing_provider_candidate: false,
                directory_observation: false,
                missing_provider_scope: None,
                auto_import_provider_observation: false,
                auto_import_scopes: Vec::new(),
            }
        } else {
            let allow_legacy_payload =
                matches!(path_entry.provenance, ProjectPathProvenance::LegacyNative);
            let disk = match read_disk_source(
                &path,
                input.options.limits.max_file_bytes,
                &read_policy,
                &path_entry,
                allow_legacy_payload,
            ) {
                Ok(disk) => disk,
                Err(error) => {
                    *complete = false;
                    incomplete_reason.get_or_insert_with(|| {
                        format!("could not re-read include source {uri}: {error}")
                    });
                    continue;
                }
            };
            if disk.text != source {
                *complete = false;
                incomplete_reason.get_or_insert_with(|| {
                    format!("include source {uri} changed during expansion")
                });
                continue;
            }
            baseline.set_include_payload_dependency(&path, read_policy.clone(), path_entry.clone());
            baseline_content_hashes.insert(path_key(&path), disk.content_hash);
            SourceRecord {
                uri: uri.clone(),
                text: source.clone(),
                version: None,
                stamp: Some(disk.stamp),
                open: false,
                path: None,
                path_stamp: None,
                content_hash: Some(disk.content_hash),
                parsed_text_hash: Some(text_content_hash(&source)),
                content_bytes: None,
                candidate_membership: None,
                candidate_observations: Vec::new(),
                read_policy: Some(read_policy.clone()),
                path_entry: Some(path_entry.clone()),
                include_payload: true,
                missing_provider_candidate: false,
                directory_observation: false,
                missing_provider_scope: None,
                auto_import_provider_observation: false,
                auto_import_scopes: Vec::new(),
            }
        };

        if *retained_files >= input.options.limits.max_files
            || retained_bytes.saturating_add(source.len()) > input.options.limits.max_total_bytes
        {
            *complete = false;
            incomplete_reason.get_or_insert_with(|| {
                "retained source limits were reached while retaining include dependencies"
                    .to_string()
            });
            continue;
        }
        *retained_files = retained_files.saturating_add(1);
        *retained_bytes = retained_bytes.saturating_add(source.len());
        if is_readable_source_for_context(loader, &path, Some(context_key)) {
            readable.insert(uri.clone());
        }
        if is_editable_source_path(loader, &path) {
            editable.insert(uri.clone());
        }
        sources.insert(uri.clone(), source);
        records.insert(uri, record);
    }
    Ok(())
}

pub(crate) fn ensure_ready(snapshot: &RenameSnapshot, uri: &Url) -> Result<(), String> {
    if !snapshot.records.contains_key(uri) {
        return Err(format!(
            "rename document was not retained in the workspace snapshot: {uri}"
        ));
    }
    if !snapshot.editable.contains(uri) {
        return Err(format!(
            "rename document is outside configured workspace roots: {uri}"
        ));
    }
    if !snapshot.complete {
        let reason = snapshot
            .incomplete_reason
            .as_deref()
            .unwrap_or("bounded source discovery did not finish");
        if snapshot
            .sources
            .get(uri)
            .is_some_and(|source| may_contain_include_directive(source.as_bytes()))
        {
            return Err(format!(
                "rename workspace scan incomplete: include dependency analysis is incomplete: {reason}"
            ));
        }
        return Err(format!("rename workspace scan incomplete: {reason}"));
    }
    Ok(())
}

fn assistance_auto_import_scopes(
    workspace: &Workspace,
    priority_contexts: &HashMap<Url, ContextKey>,
    providers: &HashMap<String, Vec<Url>>,
    candidate_prefixes: &[String],
) -> Vec<AutoImportProviderScope> {
    if providers.is_empty() || candidate_prefixes.is_empty() {
        return Vec::new();
    }
    let mut provider_units = providers
        .iter()
        .filter(|(_, candidates)| {
            candidates.iter().any(|uri| {
                workspace.index.source_text(uri).is_some_and(|source| {
                    contains_any_identifier_prefix(source, candidate_prefixes)
                })
            })
        })
        .map(|(unit, _)| unit.clone())
        .collect::<Vec<_>>();
    provider_units.sort();
    provider_units.dedup();
    if provider_units.is_empty() {
        return Vec::new();
    }
    let mut contexts = priority_contexts.values().cloned().collect::<Vec<_>>();
    contexts.sort_by_key(|key| format!("{key:?}"));
    contexts.dedup();
    let mut scopes = Vec::new();
    for context_key in contexts {
        let Some(context) = workspace
            .contexts
            .get(&context_key)
            .map(|state| &state.context)
        else {
            continue;
        };
        let mut roots = context.search_paths.clone();
        roots.extend(
            context
                .main_source_entry
                .iter()
                .filter_map(|entry| entry.path.parent().map(Path::to_path_buf)),
        );
        roots.extend(
            context
                .explicit_unit_entries
                .values()
                .flatten()
                .filter_map(|entry| entry.path.parent().map(Path::to_path_buf)),
        );
        roots.extend(mapped_source_roots(context));
        if roots.is_empty() {
            roots.extend(
                workspace
                    .roots
                    .iter()
                    .flat_map(|root| root.source_roots.iter().cloned()),
            );
        }
        roots.sort_by(|left, right| left.to_string_lossy().cmp(&right.to_string_lossy()));
        roots.dedup_by(|left, right| paths_equal_ci(left, right));
        for root in roots {
            let root = absolute_path(root);
            let path_entry = context
                .search_path_entries
                .iter()
                .find(|entry| paths_equal_ci(&entry.path, &root))
                .cloned()
                .or_else(|| super::context_path_entry(context, &root))
                .unwrap_or_else(|| ProjectPathEntry {
                    path: root.clone(),
                    provenance: ProjectPathProvenance::Configured,
                });
            scopes.push(AutoImportProviderScope {
                root,
                provider_units: provider_units.clone(),
                candidate_prefixes: candidate_prefixes.to_vec(),
                read_policy: context.read_policy.clone(),
                path_entry,
            });
        }
    }
    scopes
}

fn retain_resolvable_auto_import_providers(
    loader: &mut Workspace,
    current_uri: &Url,
    context_key: &ContextKey,
    providers: &mut HashMap<String, Vec<Url>>,
    pinned: &mut HashSet<Url>,
    candidate_names: &[String],
    cancel: &AtomicBool,
) -> Result<(), String> {
    let Some(context) = loader
        .contexts
        .get(context_key)
        .map(|state| state.context.clone())
    else {
        providers.clear();
        return Ok(());
    };
    let names = providers.keys().cloned().collect::<Vec<_>>();
    for name in names {
        if is_cancelled(cancel) {
            return Err(CANCELLATION_MESSAGE.to_string());
        }
        let Some(candidates) = providers.get(&name) else {
            continue;
        };
        if candidates.len() != 1 {
            providers.remove(&name);
            continue;
        }
        let expected = candidates[0].clone();
        let has_candidate_prefix = candidates.iter().any(|uri| {
            loader
                .index
                .source_text(uri)
                .is_some_and(|source| contains_any_identifier_prefix(source, candidate_names))
        });
        if !has_candidate_prefix {
            continue;
        }
        let lookup_name = super::aliased_unit_name(&context, &name);
        let resolved = loader.resolve_unit_with_cancel(
            current_uri,
            &name,
            &lookup_name,
            &context,
            context_key,
            pinned,
            Some(cancel),
        )?;
        if resolved.as_ref() != Some(&expected) {
            providers.remove(&name);
        }
    }
    Ok(())
}

fn enumerate_sources(
    workspace: &mut Workspace,
    input: &WorkspaceInput,
    priority: &[Url],
    priority_contexts: &HashMap<Url, ContextKey>,
    candidate_names: &[String],
    mode: SnapshotMode,
    cancel: &AtomicBool,
) -> Result<Enumeration, String> {
    let mut result = Enumeration {
        complete: true,
        auto_import_complete: true,
        ..Enumeration::default()
    };
    if matches!(mode, SnapshotMode::Local | SnapshotMode::LocalWithImports) {
        for uri in priority {
            if is_cancelled(cancel) {
                return Err(CANCELLATION_MESSAGE.to_string());
            }
            let Ok(path) = uri.to_file_path() else {
                continue;
            };
            let path = absolute_path(path);
            if !workspace.accepts_path(&path) || !is_pascal_path(&path) {
                continue;
            }
            if !is_safe_source_path(workspace, &path) {
                return Err(format!(
                    "rename source path is a symlink or escapes configured workspace roots: {uri}"
                ));
            }
            if path.is_file() || input.overlays.contains_key(uri) {
                add_baseline_path(&mut result.baseline, path.clone());
                result.add_path(path, None);
            }
        }
        if priority.iter().any(|uri| {
            uri.to_file_path().ok().is_some_and(|path| {
                path.extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("inc"))
            })
        }) {
            let root_paths = workspace
                .roots
                .iter()
                .map(|root| root.path.clone())
                .collect::<Vec<_>>();
            for root_path in root_paths {
                if is_cancelled(cancel) {
                    return Err(CANCELLATION_MESSAGE.to_string());
                }
                let catalogue =
                    workspace.filename_catalogue_with_cancel(&root_path, Some(cancel))?;
                if !catalogue.complete {
                    result.complete = false;
                    result.reason.get_or_insert_with(|| {
                        format!(
                            "include owner discovery was incomplete under {}",
                            root_path.display()
                        )
                    });
                }
                for (directory, stamp) in catalogue.directories {
                    add_baseline_path_with_stamp(&mut result.baseline, directory, stamp);
                }
                let mut owner_paths = catalogue
                    .entries
                    .into_values()
                    .flatten()
                    .map(absolute_path)
                    .collect::<Vec<_>>();
                owner_paths.sort_by_key(|path| path_key(path));
                owner_paths.dedup_by(|left, right| paths_equal_ci(left, right));
                if owner_paths.len() > MAX_RENAME_INCLUDE_OWNER_DISCOVERY {
                    result.complete = false;
                    result.reason.get_or_insert_with(|| {
                        format!(
                            "include owner discovery limit ({MAX_RENAME_INCLUDE_OWNER_DISCOVERY}) reached under {}",
                            root_path.display()
                        )
                    });
                }
                for path in owner_paths
                    .into_iter()
                    .take(MAX_RENAME_INCLUDE_OWNER_DISCOVERY)
                {
                    if is_cancelled(cancel) {
                        return Err(CANCELLATION_MESSAGE.to_string());
                    }
                    if workspace.accepts_path(&path) && is_safe_source_path(workspace, &path) {
                        add_baseline_path(&mut result.baseline, path.clone());
                        result.add_path(path, None);
                    }
                }
            }
        }
        return Ok(result);
    }
    if mode == SnapshotMode::Assistance {
        if !candidate_names.is_empty() {
            enumerate_assistance_provider_sources(
                workspace,
                input,
                priority_contexts,
                &mut result,
                cancel,
            )?;
        }
        return Ok(result);
    }
    let entry_limit = MAX_RENAME_TRAVERSAL_ENTRIES;
    let mut stop = false;
    for root in &workspace.roots {
        for source_root in &root.source_roots {
            if is_cancelled(cancel) {
                return Err(CANCELLATION_MESSAGE.to_string());
            }
            let source_root = absolute_path(source_root.clone());
            let walker = WalkDir::new(&source_root)
                .follow_links(false)
                .into_iter()
                .filter_entry(|entry| {
                    entry.depth() == 0 || !root.excludes.is_excluded(entry.path(), &source_root)
                });
            for entry in walker {
                if is_cancelled(cancel) {
                    return Err(CANCELLATION_MESSAGE.to_string());
                }
                result.visited_entries = result.visited_entries.saturating_add(1);
                if result.visited_entries > entry_limit {
                    result.complete = false;
                    result.reason.get_or_insert_with(|| {
                        format!("source traversal entry limit ({entry_limit}) reached")
                    });
                    stop = true;
                    break;
                }
                let entry = match entry {
                    Ok(entry) => entry,
                    Err(error) => {
                        result.complete = false;
                        result.reason.get_or_insert_with(|| {
                            format!(
                                "source traversal error under {}: {error}",
                                source_root.display()
                            )
                        });
                        continue;
                    }
                };
                let path = absolute_path(entry.path().to_path_buf());
                let file_type = entry.file_type();
                if file_type.is_symlink() {
                    continue;
                }
                if file_type.is_dir() {
                    add_baseline_path(&mut result.baseline, path);
                    continue;
                }
                if file_type.is_file()
                    && root.accepts(&path)
                    && is_project_metadata_path(&path)
                    && !is_configuration_file(&path)
                {
                    add_baseline_path(&mut result.baseline, path.clone());
                }
                if !file_type.is_file() || !is_pascal_path(&path) || !root.accepts(&path) {
                    continue;
                }
                add_baseline_path(&mut result.baseline, path.clone());
                result.add_path(path, None);
            }
            if stop {
                break;
            }
        }
        if stop {
            break;
        }
    }

    for uri in priority {
        let Ok(path) = uri.to_file_path() else {
            continue;
        };
        let path = absolute_path(path);
        if !workspace.accepts_path(&path) || !is_pascal_path(&path) {
            continue;
        }
        if !is_safe_source_path(workspace, &path) {
            return Err(format!(
                "rename source path is a symlink or escapes configured workspace roots: {uri}"
            ));
        }
        if path.is_file() || input.overlays.contains_key(uri) {
            add_baseline_path(&mut result.baseline, path.clone());
            result.add_path(path, None);
        }
    }
    for (uri, overlay) in &input.overlays {
        let Ok(path) = uri.to_file_path() else {
            continue;
        };
        let path = absolute_path(path);
        if workspace.accepts_path(&path) && is_pascal_path(&path) {
            if !is_safe_source_path(workspace, &path) {
                result.complete = false;
                result.reason.get_or_insert_with(|| {
                    format!("source path is a symlink or escapes configured workspace roots: {uri}")
                });
            } else if overlay.text.len() <= input.options.limits.max_file_bytes {
                add_baseline_path(&mut result.baseline, path.clone());
                result.add_path(path, None);
            } else {
                result.complete = false;
                result.reason.get_or_insert_with(|| {
                    format!("open overlay {uri} exceeds the configured per-file limit")
                });
            }
        }
    }

    result.sort_paths(priority);

    Ok(result)
}

fn enumerate_assistance_provider_sources(
    workspace: &mut Workspace,
    input: &WorkspaceInput,
    priority_contexts: &HashMap<Url, ContextKey>,
    enumeration: &mut Enumeration,
    cancel: &AtomicBool,
) -> Result<(), String> {
    let mut contexts = priority_contexts.values().cloned().collect::<Vec<_>>();
    contexts.sort_by_key(|key| format!("{key:?}"));
    contexts.dedup();

    for context_key in contexts {
        let Some(context) = workspace
            .contexts
            .get(&context_key)
            .map(|state| state.context.clone())
        else {
            enumeration.auto_import_complete = false;
            continue;
        };
        let mut context_roots = context.search_paths.clone();
        context_roots.extend(
            context
                .main_source_entry
                .iter()
                .filter_map(|entry| entry.path.parent().map(Path::to_path_buf)),
        );
        context_roots.extend(
            context
                .explicit_unit_entries
                .values()
                .flatten()
                .filter_map(|entry| entry.path.parent().map(Path::to_path_buf)),
        );
        context_roots.extend(mapped_source_roots(&context));
        if context_roots.is_empty() {
            context_roots.extend(
                workspace
                    .roots
                    .iter()
                    .flat_map(|root| root.source_roots.iter().cloned()),
            );
        }
        context_roots.sort_by(|left, right| left.to_string_lossy().cmp(&right.to_string_lossy()));
        context_roots.dedup_by(|left, right| paths_equal_ci(left, right));

        for root in context_roots {
            if is_cancelled(cancel) {
                return Err(CANCELLATION_MESSAGE.to_string());
            }
            let root = absolute_path(root);
            let catalogue = workspace.filename_catalogue_with_cancel(&root, Some(cancel))?;
            if !catalogue.complete {
                enumeration.auto_import_complete = false;
            }
            for (directory, stamp) in catalogue.directories {
                add_baseline_path_with_stamp(&mut enumeration.baseline, directory, stamp);
            }
            let mut provider_paths = catalogue
                .entries
                .into_values()
                .flatten()
                .map(absolute_path)
                .collect::<Vec<_>>();
            provider_paths.sort_by_key(|left| path_key(left));
            provider_paths.dedup_by(|left, right| paths_equal_ci(left, right));
            for path in provider_paths {
                if is_cancelled(cancel) {
                    return Err(CANCELLATION_MESSAGE.to_string());
                }
                if enumeration.paths.len() >= MAX_AUTO_IMPORT_PROVIDER_SOURCES {
                    enumeration.auto_import_complete = false;
                    return Ok(());
                }
                if !is_pascal_path(&path)
                    || !workspace.ensure_supported_project_context(
                        &path,
                        &context,
                        Some(&context_key),
                    )
                {
                    continue;
                }
                enumeration.add_path(path, Some(context_key.clone()));
            }

            let mut overlay_paths = Vec::new();
            for (uri, overlay) in &input.overlays {
                if is_cancelled(cancel) {
                    return Err(CANCELLATION_MESSAGE.to_string());
                }
                let Some(path) = uri.to_file_path().ok() else {
                    continue;
                };
                overlay_paths.push((absolute_path(path), overlay.text.len()));
            }
            overlay_paths.sort_by(|left, right| path_key(&left.0).cmp(&path_key(&right.0)));
            overlay_paths.dedup_by(|left, right| paths_equal_ci(&left.0, &right.0));
            for (path, overlay_bytes) in overlay_paths {
                if is_cancelled(cancel) {
                    return Err(CANCELLATION_MESSAGE.to_string());
                }
                if !is_pascal_path(&path)
                    || !path_starts_with_native(&path, &root)
                    || !workspace.ensure_supported_project_context(
                        &path,
                        &context,
                        Some(&context_key),
                    )
                {
                    continue;
                }
                if overlay_bytes > input.options.limits.max_file_bytes {
                    enumeration.auto_import_complete = false;
                    continue;
                }
                if enumeration.paths.len() >= MAX_AUTO_IMPORT_PROVIDER_SOURCES {
                    enumeration.auto_import_complete = false;
                    return Ok(());
                }
                enumeration.add_path(path, Some(context_key.clone()));
            }
        }
    }
    Ok(())
}

fn add_baseline_path(baseline: &mut BaselineAccumulator, path: PathBuf) {
    let stamp = path_stamp(&path);
    baseline.add_path(path, stamp);
}

fn add_baseline_path_with_stamp(
    baseline: &mut BaselineAccumulator,
    path: PathBuf,
    stamp: Option<PathStamp>,
) {
    baseline.add_path(path, stamp);
}

fn add_baseline_candidate_membership(
    baseline: &mut BaselineAccumulator,
    path: PathBuf,
    membership: ProjectCandidateMembership,
    observe_directory_stamp: bool,
) {
    baseline.add_candidate_membership(path, membership, observe_directory_stamp);
}

fn add_baseline_content_hash(
    path: &Path,
    baseline: &mut BaselineAccumulator,
    content_hashes: &mut HashMap<String, u64>,
    read_policy: &ReadPolicy,
    path_entry: &ProjectPathEntry,
    cancel: &AtomicBool,
    require_file: bool,
) -> Result<(), String> {
    if is_cancelled(cancel) {
        return Err(CANCELLATION_MESSAGE.to_string());
    }
    let key = path_key(path);
    if content_hashes.contains_key(&key) {
        baseline.set_payload_dependency(path, read_policy.clone(), path_entry.clone());
        return Ok(());
    }
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if require_file {
                return Err(format!(
                    "{} disappeared while fingerprinting",
                    path.display()
                ));
            }
            return Ok(());
        }
        Err(error) => {
            return Err(format!("cannot inspect {}: {error}", path.display()));
        }
    };
    if !metadata.is_file() {
        if require_file {
            return Err(format!("{} is not a regular file", path.display()));
        }
        return Ok(());
    }
    let content_hash = file_content_hash(path, read_policy, path_entry, cancel)?;
    content_hashes.insert(key, content_hash);
    baseline.set_payload_dependency(path, read_policy.clone(), path_entry.clone());
    Ok(())
}

fn read_exact_file_bytes(
    path: &Path,
    read_policy: &ReadPolicy,
    entry: &ProjectPathEntry,
    cancel: &AtomicBool,
) -> Result<Vec<u8>, String> {
    if is_cancelled(cancel) {
        return Err(CANCELLATION_MESSAGE.to_string());
    }
    let bytes = read_policy
        .read_payload_bytes(entry, MAX_RENAME_CONFIG_BYTES as u64)
        .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    if is_cancelled(cancel) {
        return Err(CANCELLATION_MESSAGE.to_string());
    }
    Ok(bytes)
}

fn capture_context_baseline(
    workspace: &Workspace,
    context_key: &super::ContextKey,
    baseline: &mut BaselineAccumulator,
    baseline_content_hashes: &mut HashMap<String, u64>,
    baseline_contents: &mut HashMap<String, Vec<u8>>,
    observe_directory_stamps: bool,
    cancel: &AtomicBool,
) -> Result<(), String> {
    if let Some(state) = workspace.contexts.get(context_key) {
        for (directory, membership) in &state.project_candidate_memberships {
            let membership = match membership {
                Ok(membership) => membership,
                Err(error) if error == CANCELLATION_MESSAGE => return Err(error.clone()),
                Err(error) => {
                    return Err(format!(
                        "project candidate membership could not be observed for {}: {error}",
                        directory.display()
                    ));
                }
            };
            add_baseline_candidate_membership(
                baseline,
                directory.clone(),
                membership.clone(),
                observe_directory_stamps && workspace.accepts_path(directory),
            );
        }
        for observation in &state.project_read_observations {
            if is_cancelled(cancel) {
                return Err(CANCELLATION_MESSAGE.to_string());
            }
            let path = &observation.path;
            let stamp = Some(super::path_stamp_from_project_read(&observation.stamp));
            add_baseline_path_with_stamp(baseline, path.clone(), stamp.clone());
            baseline_content_hashes
                .entry(path_key(path))
                .or_insert(observation.content_hash);
            if let Some(content_bytes) = &observation.content_bytes {
                baseline_contents
                    .entry(path_key(path))
                    .or_insert_with(|| content_bytes.clone());
            }
            if let Some(path_entry) = super::context_path_entry(&state.context, path) {
                baseline.set_payload_observation(
                    path,
                    stamp,
                    state.context.read_policy.clone(),
                    path_entry,
                );
            }
        }
        for observation in &state.context.metadata_observations {
            if is_cancelled(cancel) {
                return Err(CANCELLATION_MESSAGE.to_string());
            }
            let path = observation.path();
            match observation {
                pascal_project::MetadataObservation::Stat { .. } => {
                    add_baseline_path_with_stamp(baseline, path.to_path_buf(), path_stamp(path));
                }
                pascal_project::MetadataObservation::Payload {
                    read_policy,
                    path_entry,
                    stamp,
                    content_hash,
                    ..
                } => {
                    baseline.set_payload_observation(
                        path,
                        stamp.clone(),
                        read_policy.clone(),
                        path_entry.clone(),
                    );
                    baseline_content_hashes
                        .entry(path_key(path))
                        .or_insert(*content_hash);
                }
            }
        }
    }
    Ok(())
}

fn capture_consumed_configuration_baseline(
    records: &[SourceRecord],
    baseline: &mut BaselineAccumulator,
    baseline_content_hashes: &mut HashMap<String, u64>,
    baseline_contents: &mut HashMap<String, Vec<u8>>,
    cancel: &AtomicBool,
) -> Result<(), String> {
    for record in records {
        if is_cancelled(cancel) {
            return Err(CANCELLATION_MESSAGE.to_string());
        }
        let Some(path) = record.path.as_ref() else {
            continue;
        };
        if let Some(membership) = &record.candidate_membership {
            add_baseline_candidate_membership(
                baseline,
                path.clone(),
                membership.clone(),
                record.path_stamp.is_some(),
            );
            continue;
        }
        add_baseline_path_with_stamp(baseline, path.clone(), record.path_stamp.clone());
        if let Some(content_hash) = record.content_hash {
            baseline_content_hashes
                .entry(path_key(path))
                .or_insert(content_hash);
        }
        if let Some(content_bytes) = &record.content_bytes {
            baseline_contents
                .entry(path_key(path))
                .or_insert_with(|| content_bytes.clone());
            if let Ok((read_policy, path_entry)) = record.payload_dependency() {
                baseline.set_payload_dependency(path, read_policy.clone(), path_entry.clone());
            }
        }
    }
    Ok(())
}

#[cfg(windows)]
fn path_key(path: &Path) -> String {
    path.to_string_lossy().to_ascii_lowercase()
}

#[cfg(not(windows))]
fn path_key(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn is_project_metadata_path(path: &Path) -> bool {
    path.extension().is_some_and(|extension| {
        matches!(
            extension.to_string_lossy().to_ascii_lowercase().as_str(),
            "dproj" | "dpr" | "dpk" | "optset" | "props" | "targets" | "toml" | "xml"
        )
    })
}

fn is_safe_source_path(workspace: &Workspace, path: &Path) -> bool {
    let Some(canonical_path) = canonical_source_path(path) else {
        return false;
    };
    workspace.roots.iter().any(|root| {
        root.source_roots.iter().any(|source_root| {
            let Some(canonical_root) = fs::canonicalize(source_root).ok() else {
                return false;
            };
            path_starts_with_ci(&canonical_path, &canonical_root)
                && !has_symlink_component(path, source_root)
        })
    })
}

fn is_editable_source_path(workspace: &Workspace, path: &Path) -> bool {
    workspace
        .roots
        .iter()
        .any(|root| path_starts_with_ci(path, &root.path) && root.accepts(path))
        && is_safe_source_path(workspace, path)
}

fn is_readable_source_path(workspace: &Workspace, path: &Path) -> bool {
    workspace.accepts_path(path) && is_safe_source_path(workspace, path)
}

fn is_readable_source_for_context(
    workspace: &Workspace,
    path: &Path,
    context_key: Option<&ContextKey>,
) -> bool {
    context_key
        .and_then(|key| {
            workspace
                .contexts
                .get(key)
                .map(|state| (key, &state.context))
        })
        .is_some_and(|(key, context)| {
            let legacy_route = Url::from_file_path(path)
                .ok()
                .is_some_and(|uri| workspace.legacy_route_is_current(&uri, path, key));
            workspace.ensure_supported_project_context_with_legacy_route(
                path,
                context,
                Some(key),
                legacy_route,
            )
        })
        || is_readable_source_path(workspace, path)
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
    let Ok(relative) = path.strip_prefix(root) else {
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

pub(crate) fn workspace_edit(
    raw_edits: HashMap<Url, Vec<TextEdit>>,
    records: &HashMap<Url, SourceRecord>,
    document_changes: bool,
) -> Result<WorkspaceEdit, String> {
    let mut entries: Vec<(Url, Vec<TextEdit>)> = raw_edits.into_iter().collect();
    entries.sort_by(|left, right| left.0.as_str().cmp(right.0.as_str()));
    if document_changes {
        let edits = entries
            .into_iter()
            .map(|(uri, edits)| {
                let version = records.get(&uri).and_then(|record| record.version);
                TextDocumentEdit {
                    text_document: OptionalVersionedTextDocumentIdentifier { uri, version },
                    edits: edits.into_iter().map(OneOf::Left).collect(),
                }
            })
            .collect();
        Ok(WorkspaceEdit {
            changes: None,
            document_changes: Some(DocumentChanges::Edits(edits)),
            change_annotations: None,
        })
    } else {
        Ok(WorkspaceEdit {
            changes: Some(entries.into_iter().collect()),
            document_changes: None,
            change_annotations: None,
        })
    }
}

pub(crate) fn check_includes(
    snapshot: &RenameSnapshot,
    uri: &Url,
    position: Position,
    candidate_names: &[String],
    cancel: Option<&AtomicBool>,
) -> Result<(), String> {
    if cancel.is_some_and(is_cancelled) {
        return Err(CANCELLATION_MESSAGE.to_string());
    }
    if let Some(error) = snapshot.include_errors.first() {
        return Err(error.clone());
    }
    let target_name = snapshot
        .sources
        .get(uri)
        .and_then(|source| identifier_at_position(source, position));
    for (source_uri, source) in &snapshot.sources {
        if cancel.is_some_and(is_cancelled) {
            return Err(CANCELLATION_MESSAGE.to_string());
        }
        let relevant = source_uri == uri || contains_any_identifier(source, candidate_names);
        if relevant {
            let target_is_unknown = source_uri == uri
                && text::position_to_offset(source, position).is_some_and(|offset| {
                    snapshot.index.conditional_unknown_at(source_uri, offset)
                });
            let unknown_candidate = target_name.as_deref().is_some_and(|name| {
                snapshot
                    .index
                    .conditional_unknown_contains_identifier(source_uri, name)
            });
            if target_is_unknown || unknown_candidate {
                return Err(format!(
                    "rename cannot prove completeness because relevant conditional compilation affects {source_uri}"
                ));
            }
            if cancel.is_some_and(is_cancelled) {
                return Err(CANCELLATION_MESSAGE.to_string());
            }
            if snapshot
                .index
                .conditional_analysis(source_uri)
                .is_some_and(|analysis| {
                    analysis.pascal_condition_contains_identifier(candidate_names)
                })
            {
                return Err(format!(
                    "rename cannot prove completeness because a Pascal conditional expression affects {source_uri}"
                ));
            }
        }
    }
    Ok(())
}

#[derive(Debug, Default)]
struct IncludeAuditResult {
    errors: Vec<String>,
    incomplete_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum IncludeRoute {
    Legacy,
    Mapped { root: PathBuf },
}

#[derive(Debug)]
struct IncludeLookup {
    observations: Vec<IncludeObservation>,
    selected: Option<PathBuf>,
    selected_directory: Option<PathBuf>,
    selected_route: IncludeRoute,
    error: Option<String>,
}

#[derive(Debug, Clone)]
struct IncludeObservation {
    path: PathBuf,
    stamp: Option<PathStamp>,
}

#[derive(Debug, Clone)]
struct IncludeAnalysis {
    safe: bool,
    relevant: bool,
    reason: Option<String>,
    environment: Option<conditional::ConditionalEnvironment>,
}

impl IncludeAnalysis {
    fn safe_with_environment(
        relevant: bool,
        environment: Option<conditional::ConditionalEnvironment>,
    ) -> Self {
        Self {
            safe: true,
            relevant,
            reason: None,
            environment,
        }
    }

    fn unsafe_with_reason(reason: impl Into<String>, relevant: bool) -> Self {
        Self {
            safe: false,
            relevant,
            reason: Some(reason.into()),
            environment: None,
        }
    }
}

struct IncludeAuditor<'a> {
    input: &'a WorkspaceInput,
    sources: &'a HashMap<Url, String>,
    loader: &'a mut Workspace,
    contexts: &'a mut HashMap<Url, ContextKey>,
    baseline: &'a mut BaselineAccumulator,
    baseline_content_hashes: &'a mut HashMap<String, u64>,
    baseline_contents: &'a mut HashMap<String, Vec<u8>>,
    candidate_names: &'a [String],
    max_file_bytes: usize,
    observe_directory_stamps: bool,
    allow_incomplete_context_for: &'a [Url],
    cancel: &'a AtomicBool,
    cache: HashMap<String, IncludeAnalysis>,
    resolution_cache: HashMap<String, Resolution<LoadedSource>>,
    resolvers:
        HashMap<ContextKey, pascal_core::resolver::UnitResolver<shared_resolver::LspSourceStore>>,
    active: HashSet<String>,
    name_free_assistance: bool,
    files_read: usize,
    bytes_read: usize,
    directives_seen: usize,
    stopped: bool,
    result: IncludeAuditResult,
}

struct IncludeInspection<'a> {
    context_key: &'a ContextKey,
    context: &'a ProjectContext,
    owner_path: &'a Path,
    legacy_route: Option<LegacyRoute>,
    conditional_environment: Option<conditional::ConditionalEnvironment>,
}

fn audit_includes(mut auditor: IncludeAuditor<'_>) -> Result<IncludeAuditResult, String> {
    let mut uris = auditor.sources.keys().cloned().collect::<Vec<_>>();
    uris.sort_by(|left, right| left.as_str().cmp(right.as_str()));
    for uri in uris {
        if auditor.stopped {
            break;
        }
        // Expansion already audited this physical include through each owning
        // virtual root. Auditing it again as a standalone document loses the
        // owner's legacy route and can falsely report a nested include as
        // unresolved.
        if auditor.loader.include_parents.contains_key(&uri) {
            continue;
        }
        if is_cancelled(auditor.cancel) {
            return Err(CANCELLATION_MESSAGE.to_string());
        }
        let context = auditor.context_for_source(&uri)?;
        let Some(source) = auditor.sources.get(&uri).cloned() else {
            continue;
        };
        let conditional_context = context
            .as_ref()
            .map(|(_, context)| {
                if !context.discovery_complete
                    && auditor
                        .allow_incomplete_context_for
                        .iter()
                        .any(|allowed_uri| allowed_uri == &uri)
                {
                    ConditionalContext::default()
                } else {
                    context.effective_conditional_context()
                }
            })
            .unwrap_or_else(ConditionalContext::default);
        let cancel = auditor.cancel;
        #[cfg(test)]
        if TEST_CANCEL_INCLUDE_ANALYSIS.with(Cell::get) {
            cancel.store(true, Ordering::Relaxed);
        }
        let incomplete_context = context
            .as_ref()
            .is_none_or(|(_, context)| !context.discovery_complete);
        let allow_incomplete_context = auditor
            .allow_incomplete_context_for
            .iter()
            .any(|allowed_uri| allowed_uri == &uri);
        let preliminary =
            conditional::analyze_with_context_and_cancel(&source, &conditional_context, cancel);
        if is_cancelled(cancel) {
            return Err(CANCELLATION_MESSAGE.to_string());
        }
        let has_potentially_active_include = preliminary.directives.iter().any(|directive| {
            directive.kind == ConditionalDirectiveKind::Include && directive.potentially_active()
        });
        if incomplete_context && allow_incomplete_context && has_potentially_active_include {
            auditor.record_error(format!(
                "rename cannot prove completeness because potentially active include in {uri} depends on incomplete project search paths"
            ));
            auditor.stopped = true;
            break;
        }

        let Some((context_key, context)) = context else {
            auditor.record_error(format!(
                "rename cannot prove completeness because include owner {uri} has no project context"
            ));
            auditor.stopped = true;
            break;
        };
        let Some(mut environment) =
            conditional::ConditionalEnvironment::try_from_context(&conditional_context)
        else {
            auditor.record_error(format!(
                "rename cannot prove completeness because conditional environment exceeds evaluator bounds for {uri}"
            ));
            auditor.stopped = true;
            break;
        };
        let mut callback_error = None;
        let mut include =
            |directive: &ConditionalDirective,
             environment: &mut conditional::ConditionalEnvironment| {
                if auditor.stopped {
                    return conditional::IncludeTransition {
                        complete: false,
                        environment_known: false,
                    };
                }
                if is_cancelled(cancel) {
                    callback_error = Some(CANCELLATION_MESSAGE.to_string());
                    return conditional::IncludeTransition {
                        complete: false,
                        environment_known: false,
                    };
                }
                if !auditor.take_directive_budget() {
                    auditor.record_error(format!(
                    "rename cannot prove completeness because include directive limit ({MAX_RENAME_INCLUDE_DIRECTIVES}) was reached"
                ));
                    auditor.stopped = true;
                    return conditional::IncludeTransition {
                        complete: false,
                        environment_known: false,
                    };
                }
                let directive = legacy_directive(directive);
                let analysis = match auditor.inspect_top_level(
                    &uri,
                    &directive,
                    &context_key,
                    &context,
                    Some(environment.clone()),
                ) {
                    Ok(analysis) => analysis,
                    Err(error) => {
                        callback_error = Some(error);
                        return conditional::IncludeTransition {
                            complete: false,
                            environment_known: false,
                        };
                    }
                };
                if let Some(next_environment) = analysis.environment {
                    *environment = next_environment;
                    conditional::IncludeTransition {
                        complete: analysis.safe,
                        environment_known: analysis.safe,
                    }
                } else {
                    conditional::IncludeTransition {
                        complete: false,
                        environment_known: false,
                    }
                }
            };
        let conditional = conditional::analyze_with_include_callback(
            &source,
            &mut environment,
            cancel,
            &mut include,
        );
        if let Some(error) = callback_error {
            return Err(error);
        }
        if is_cancelled(cancel) {
            return Err(CANCELLATION_MESSAGE.to_string());
        }
        if !conditional.complete {
            auditor.result.incomplete_reason.get_or_insert_with(|| {
                format!("conditional analysis is incomplete for include owner {uri}")
            });
            if auditor.stopped {
                break;
            }
            continue;
        }
        if conditional.directives.iter().any(|directive| {
            directive.potentially_active() && directive.kind == ConditionalDirectiveKind::Other
        }) {
            auditor.record_error(format!(
                "rename cannot prove completeness because include owner {uri} contains an unsupported directive"
            ));
            auditor.stopped = true;
            break;
        }
    }

    Ok(auditor.result)
}

impl IncludeAuditor<'_> {
    fn context_for_source(
        &mut self,
        uri: &Url,
    ) -> Result<Option<(ContextKey, ProjectContext)>, String> {
        let context_key = if let Some(context_key) = self.contexts.get(uri).cloned() {
            Some(context_key)
        } else if let Some(context_key) = self.loader.document_contexts.get(uri).cloned() {
            self.contexts.insert(uri.clone(), context_key.clone());
            Some(context_key)
        } else {
            let context_key = self
                .loader
                .context_for_uri_with_cancel(uri, Some(self.cancel))?;
            self.contexts.insert(uri.clone(), context_key.clone());
            Some(context_key)
        };

        let Some(context_key) = context_key else {
            return Ok(None);
        };
        let workspace: &Workspace = &*self.loader;
        capture_context_baseline(
            workspace,
            &context_key,
            self.baseline,
            self.baseline_content_hashes,
            self.baseline_contents,
            self.observe_directory_stamps,
            self.cancel,
        )?;
        let Some(state) = self.loader.contexts.get(&context_key) else {
            return Ok(None);
        };
        let context = state.context.clone();
        let allow_incomplete_context = self
            .allow_incomplete_context_for
            .iter()
            .any(|allowed_uri| allowed_uri == uri);
        if !context.discovery_complete && !allow_incomplete_context {
            self.result.incomplete_reason.get_or_insert_with(|| {
                format!("project context is ambiguous or incomplete for {uri}")
            });
        }
        Ok(Some((context_key, context)))
    }

    fn take_directive_budget(&mut self) -> bool {
        if self.directives_seen >= MAX_RENAME_INCLUDE_DIRECTIVES {
            return false;
        }
        self.directives_seen += 1;
        true
    }

    fn top_level_legacy_route(
        &self,
        owner_path: &Path,
        context_key: &ContextKey,
        context: &ProjectContext,
    ) -> Option<LegacyRoute> {
        let uri = Url::from_file_path(owner_path).ok()?;
        self.loader
            .legacy_route_for_resolver(&uri, owner_path, context_key, context)
            .or_else(|| {
                owner_has_legacy_or_workspace_authority(self.loader, owner_path, context).then(
                    || LegacyRoute {
                        source_path: owner_path.to_path_buf(),
                        sibling_directory: owner_path.parent().unwrap_or(owner_path).to_path_buf(),
                    },
                )
            })
    }

    fn legacy_route_for_loaded_source(
        &self,
        context: &ProjectContext,
        source: &LoadedSource,
        inherited: Option<&LegacyRoute>,
    ) -> Option<LegacyRoute> {
        let legacy = match &source.revision {
            pascal_core::resolver::SourceRevision::Disk { path_entry, .. } => {
                matches!(path_entry.provenance, ProjectPathProvenance::LegacyNative)
            }
            pascal_core::resolver::SourceRevision::Overlay { .. } => {
                context.path_entry_for(&source.path).is_some_and(|entry| {
                    matches!(entry.provenance, ProjectPathProvenance::LegacyNative)
                }) || inherited.is_some_and(|route| {
                    source.path.parent().is_some_and(|parent| {
                        path_key(parent) == path_key(&route.sibling_directory)
                    })
                })
            }
        };
        legacy.then(|| LegacyRoute {
            source_path: source.path.clone(),
            sibling_directory: source.path.parent().unwrap_or(&source.path).to_path_buf(),
        })
    }

    fn resolve_include(
        &mut self,
        context_key: &ContextKey,
        context: &ProjectContext,
        including_path: &Path,
        directive: &Directive,
        legacy_route: Option<&LegacyRoute>,
    ) -> Result<Resolution<LoadedSource>, String> {
        let Some(requested_name) = include_name(directive) else {
            return Ok(Resolution::Unavailable {
                reason: "include directive has no statically known file name".to_string(),
            });
        };
        let cache_key = include_resolution_cache_key(
            context_key,
            including_path,
            &requested_name,
            legacy_route,
        );
        if let Some(result) = self.resolution_cache.get(&cache_key) {
            return Ok(result.clone());
        }

        let limits = ResolverLimits {
            max_source_bytes: self.max_file_bytes,
            max_include_files: MAX_RENAME_INCLUDE_FILES,
            max_include_bytes: MAX_RENAME_INCLUDE_BYTES,
            max_include_directives: MAX_RENAME_INCLUDE_DIRECTIVES,
            max_include_depth: MAX_RENAME_INCLUDE_DEPTH,
            ..ResolverLimits::default()
        };
        let (outcome, report) = {
            let resolver = self
                .resolvers
                .entry(context_key.clone())
                .or_insert_with(|| {
                    shared_resolver::resolver_for_context_with_limits(
                        context.clone(),
                        self.input.roots.clone(),
                        self.input,
                        limits,
                    )
                });
            let (used_files, used_bytes, used_directives) = resolver.include_usage();
            resolver.set_include_limits(
                used_files.saturating_add(MAX_RENAME_INCLUDE_FILES.saturating_sub(self.files_read)),
                used_bytes.saturating_add(MAX_RENAME_INCLUDE_BYTES.saturating_sub(self.bytes_read)),
                used_directives.saturating_add(
                    MAX_RENAME_INCLUDE_DIRECTIVES.saturating_sub(self.directives_seen),
                ),
            );
            let outcome = resolver.try_resolve_include(
                IncludeResolveRequest {
                    including_path,
                    byte_range: directive.start..directive.end,
                    requested_name: &requested_name,
                    legacy_route,
                },
                self.cancel,
            );
            let report = resolver.report();
            (outcome, report)
        };
        self.observe_resolver_report(context_key, &report);
        let result = match outcome {
            Ok(outcome) => match outcome.result {
                Resolution::Incomplete { reason, candidates } => {
                    let reason = if report.warnings.iter().any(|warning| {
                        let warning = warning.to_ascii_lowercase();
                        warning.contains("unauthorized")
                            || warning.contains("outside the requester-scoped")
                            || warning.contains("outside the owning project")
                    }) {
                        "include source is outside the owning project's readable roots".to_string()
                    } else {
                        reason
                    };
                    Resolution::Incomplete { reason, candidates }
                }
                result => result,
            },
            Err(pascal_core::ResolverError::Cancelled) => {
                return Err(CANCELLATION_MESSAGE.to_string());
            }
            Err(error) => Resolution::Incomplete {
                reason: error.to_string(),
                candidates: Vec::new(),
            },
        };
        self.resolution_cache.insert(cache_key, result.clone());
        Ok(result)
    }

    fn observe_resolver_report(
        &mut self,
        context_key: &ContextKey,
        report: &pascal_core::resolver::ResolutionReport,
    ) {
        self.loader.merge_resolution_report(context_key, report);
        for observation in &report.observations {
            match observation {
                pascal_core::resolver::ResolutionObservation::Directory { path, stamp, .. } => {
                    add_baseline_path_with_stamp(self.baseline, path.clone(), stamp.clone())
                }
                pascal_core::resolver::ResolutionObservation::Candidate {
                    path,
                    stamp,
                    present,
                    ..
                } => {
                    // A denied candidate may still exist on disk (for
                    // example, a configured symlink outside the requester
                    // roots). Its presence must not stale the request, while
                    // an actually missing candidate remains an observed
                    // precedence input and must stale if it appears.
                    if *present || stamp.is_some() || !path.exists() {
                        add_baseline_path_with_stamp(self.baseline, path.clone(), stamp.clone());
                    }
                }
                pascal_core::resolver::ResolutionObservation::Payload {
                    path,
                    revision: pascal_core::resolver::SourceRevision::Disk { stamp, .. },
                    ..
                } => add_baseline_path_with_stamp(self.baseline, path.clone(), Some(stamp.clone())),
                _ => {}
            }
        }
    }

    fn record_loaded_include(
        &mut self,
        context: &ProjectContext,
        source: &LoadedSource,
    ) -> Result<(), String> {
        let uri = Url::from_file_path(&source.path).map_err(|_| {
            format!(
                "include source is not a file URI: {}",
                source.path.display()
            )
        })?;
        self.loader
            .record_resolved_source(context, source, uri, true)
    }

    fn inspect_top_level(
        &mut self,
        uri: &Url,
        directive: &Directive,
        context_key: &ContextKey,
        context: &ProjectContext,
        conditional_environment: Option<conditional::ConditionalEnvironment>,
    ) -> Result<IncludeAnalysis, String> {
        let Some(owner_path) = uri.to_file_path().ok().map(absolute_path) else {
            self.record_error(format!(
                "rename cannot prove completeness because an include path in {uri} is unresolved"
            ));
            self.stopped = true;
            return Ok(IncludeAnalysis::unsafe_with_reason(
                format!("include path in {uri} is unresolved"),
                false,
            ));
        };
        let legacy_route = self.top_level_legacy_route(&owner_path, context_key, context);
        let resolution = self.resolve_include(
            context_key,
            context,
            &owner_path,
            directive,
            legacy_route.as_ref(),
        )?;
        let source = match resolution {
            Resolution::Found(source) => source,
            Resolution::Unavailable { .. } => {
                self.record_error(format!(
                    "rename cannot prove completeness because an include path in {uri} is unresolved"
                ));
                self.stopped = true;
                return Ok(IncludeAnalysis::unsafe_with_reason(
                    format!("include path in {uri} is unresolved"),
                    false,
                ));
            }
            Resolution::Ambiguous { candidates } => {
                self.record_error(format!(
                    "rename cannot prove completeness because include in {uri} is ambiguous ({})",
                    candidates.len()
                ));
                self.stopped = true;
                return Ok(IncludeAnalysis::unsafe_with_reason(
                    format!("include in {uri} is ambiguous ({})", candidates.len()),
                    false,
                ));
            }
            Resolution::Incomplete { reason, .. } => {
                self.record_error(format!(
                    "rename cannot prove completeness because include in {uri} could not be read: {reason}"
                ));
                self.stopped = true;
                return Ok(IncludeAnalysis::unsafe_with_reason(
                    format!("include in {uri} could not be read: {reason}"),
                    false,
                ));
            }
        };
        let legacy_route =
            self.legacy_route_for_loaded_source(context, &source, legacy_route.as_ref());
        let inspection = IncludeInspection {
            context_key,
            context,
            owner_path: &owner_path,
            legacy_route,
            conditional_environment,
        };
        let analysis = self.inspect_include_file(&source, inspection, 0)?;
        if !analysis.safe || (analysis.relevant && self.name_free_assistance) {
            let reason = analysis
                .reason
                .clone()
                .unwrap_or_else(|| format!("include {:?} contains source content", source.path));
            self.record_error(format!("rename cannot prove completeness because {reason}"));
            self.stopped = true;
        }
        Ok(analysis)
    }

    #[allow(clippy::too_many_arguments)]
    fn inspect_nested(
        &mut self,
        owner_path: &Path,
        directive: &Directive,
        context_key: &ContextKey,
        context: &ProjectContext,
        inherited_route: Option<LegacyRoute>,
        conditional_environment: conditional::ConditionalEnvironment,
        depth: usize,
    ) -> Result<IncludeAnalysis, String> {
        if depth >= MAX_RENAME_INCLUDE_DEPTH {
            self.stopped = true;
            return Ok(IncludeAnalysis::unsafe_with_reason(
                format!(
                    "include in {owner_path:?} exceeds the maximum include nesting depth ({MAX_RENAME_INCLUDE_DEPTH})"
                ),
                false,
            ));
        }
        let resolution = self.resolve_include(
            context_key,
            context,
            owner_path,
            directive,
            inherited_route.as_ref(),
        )?;
        let source = match resolution {
            Resolution::Found(source) => source,
            Resolution::Unavailable { .. } => {
                return Ok(IncludeAnalysis::unsafe_with_reason(
                    format!("include in {owner_path:?} has an unresolved path"),
                    false,
                ));
            }
            Resolution::Ambiguous { .. } => {
                return Ok(IncludeAnalysis::unsafe_with_reason(
                    format!("include in {owner_path:?} has an ambiguous path"),
                    false,
                ));
            }
            Resolution::Incomplete { reason, .. } => {
                return Ok(IncludeAnalysis::unsafe_with_reason(
                    format!("include in {owner_path:?} could not be read: {reason}"),
                    false,
                ));
            }
        };
        let legacy_route =
            self.legacy_route_for_loaded_source(context, &source, inherited_route.as_ref());
        let inspection = IncludeInspection {
            context_key,
            context,
            owner_path,
            legacy_route,
            conditional_environment: Some(conditional_environment),
        };
        self.inspect_include_file(&source, inspection, depth + 1)
    }

    fn inspect_include_file(
        &mut self,
        source: &LoadedSource,
        inspection: IncludeInspection<'_>,
        depth: usize,
    ) -> Result<IncludeAnalysis, String> {
        if is_cancelled(self.cancel) {
            return Err(CANCELLATION_MESSAGE.to_string());
        }

        if depth > MAX_RENAME_INCLUDE_DEPTH {
            self.stopped = true;
            return Ok(IncludeAnalysis::unsafe_with_reason(
                format!(
                    "include {:?} exceeds the maximum include nesting depth ({MAX_RENAME_INCLUDE_DEPTH})",
                    source.path
                ),
                false,
            ));
        }
        let path = &source.path;
        let active_key = canonical_include_key(path);
        if self.active.contains(&active_key) {
            return Ok(IncludeAnalysis::unsafe_with_reason(
                format!("include {path:?} has an include cycle"),
                false,
            ));
        }
        let cache_key = include_cache_key(
            path,
            inspection.context_key,
            inspection.owner_path,
            inspection.legacy_route.as_ref(),
            inspection.conditional_environment.as_ref(),
        );
        if let Some(analysis) = self.cache.get(&cache_key) {
            return Ok(analysis.clone());
        }
        if self.files_read >= MAX_RENAME_INCLUDE_FILES {
            self.stopped = true;
            let analysis = IncludeAnalysis::unsafe_with_reason(
                format!(
                    "include {path:?} could not be audited because include file limit ({MAX_RENAME_INCLUDE_FILES}) was reached"
                ),
                false,
            );
            self.cache.insert(cache_key, analysis.clone());
            return Ok(analysis);
        }
        let remaining_bytes = MAX_RENAME_INCLUDE_BYTES.saturating_sub(self.bytes_read);
        if remaining_bytes == 0 {
            self.stopped = true;
            let analysis = IncludeAnalysis::unsafe_with_reason(
                format!(
                    "include {path:?} could not be audited because include byte limit ({MAX_RENAME_INCLUDE_BYTES}) was reached"
                ),
                false,
            );
            self.cache.insert(cache_key, analysis.clone());
            return Ok(analysis);
        }

        self.files_read += 1;
        if source.bytes.len() > remaining_bytes {
            self.stopped = true;
            let analysis = IncludeAnalysis::unsafe_with_reason(
                format!(
                    "include {path:?} could not be audited because include byte limit ({MAX_RENAME_INCLUDE_BYTES}) was reached"
                ),
                false,
            );
            self.cache.insert(cache_key, analysis.clone());
            return Ok(analysis);
        }
        self.bytes_read = self.bytes_read.saturating_add(source.bytes.len());
        self.record_loaded_include(inspection.context, source)?;
        if let pascal_core::resolver::SourceRevision::Disk {
            content_hash,
            read_policy,
            path_entry,
            ..
        } = &source.revision
        {
            self.baseline_content_hashes
                .entry(path_key(path))
                .or_insert(*content_hash);
            self.baseline.set_include_payload_dependency(
                path,
                read_policy.clone(),
                path_entry.clone(),
            );
        }

        // Use the facts established at the owning include boundary.  When the
        // boundary is unknown, the empty environment intentionally makes
        // conditional facts unknown; it must not resurrect project defines.
        let include_text = shared_resolver::decode_source_bytes(&source.bytes);
        let inherited_facts = inspection
            .conditional_environment
            .as_ref()
            .is_some_and(conditional::ConditionalEnvironment::has_facts);
        let mut environment = inspection.conditional_environment.unwrap_or_default();
        self.active.insert(active_key.clone());
        let cancel = self.cancel;
        let mut callback_error = None;
        let mut nested_failure = None;
        let mut nested_include =
            |directive: &ConditionalDirective,
             environment: &mut conditional::ConditionalEnvironment| {
                if is_cancelled(cancel) {
                    callback_error = Some(CANCELLATION_MESSAGE.to_string());
                    return conditional::IncludeTransition {
                        complete: false,
                        environment_known: false,
                    };
                }
                if self.stopped {
                    return conditional::IncludeTransition {
                        complete: false,
                        environment_known: false,
                    };
                }
                if !self.take_directive_budget() {
                    self.stopped = true;
                    nested_failure = Some(format!(
                        "include {path:?} could not be audited because include directive limit ({MAX_RENAME_INCLUDE_DIRECTIVES}) was reached"
                    ));
                    return conditional::IncludeTransition {
                        complete: false,
                        environment_known: false,
                    };
                }
                let directive = legacy_directive(directive);
                let child = match self.inspect_nested(
                    path,
                    &directive,
                    inspection.context_key,
                    inspection.context,
                    inspection.legacy_route.clone(),
                    environment.clone(),
                    depth,
                ) {
                    Ok(child) => child,
                    Err(error) => {
                        callback_error = Some(error);
                        return conditional::IncludeTransition {
                            complete: false,
                            environment_known: false,
                        };
                    }
                };
                if !child.safe {
                    self.stopped = true;
                    nested_failure = Some(child.reason.unwrap_or_else(|| {
                        format!("include {path:?} contains unsafe nested source")
                    }));
                    return conditional::IncludeTransition {
                        complete: false,
                        environment_known: false,
                    };
                }
                let Some(next_environment) = child.environment else {
                    self.stopped = true;
                    nested_failure = Some(format!(
                        "include {path:?} did not yield a known conditional environment"
                    ));
                    return conditional::IncludeTransition {
                        complete: false,
                        environment_known: false,
                    };
                };
                *environment = next_environment;
                conditional::IncludeTransition {
                    complete: true,
                    environment_known: true,
                }
            };
        let conditional = conditional::analyze_with_include_callback(
            &include_text,
            &mut environment,
            cancel,
            &mut nested_include,
        );
        self.active.remove(&active_key);
        if let Some(error) = callback_error {
            return Err(error);
        }
        if is_cancelled(self.cancel) {
            return Err(CANCELLATION_MESSAGE.to_string());
        }
        let relevant = if self.name_free_assistance {
            projected_source_contains_pascal_tokens(&conditional.projected_source)
        } else {
            conditional.potentially_active_contains_identifier(&include_text, self.candidate_names)
                || conditional.pascal_condition_contains_identifier(self.candidate_names)
        };
        let include_establishes_facts = conditional.directives.iter().any(|directive| {
            matches!(
                directive.kind,
                ConditionalDirectiveKind::Define | ConditionalDirectiveKind::Undef
            )
        });
        // A candidate reference inside a conditional block whose activity is
        // supplied only by inherited project/owner facts is not a portable
        // physical edit proof.  An include that establishes its own facts is
        // analyzed from that local state instead; known-active self-defined
        // conditional declarations/references remain valid.
        let inherited_conditional_candidate = !self.name_free_assistance
            && inherited_facts
            && !include_establishes_facts
            && inherited_conditional_branch_contains_identifier(
                &include_text,
                &conditional,
                self.candidate_names,
            );
        let conditional_candidate_is_uncertain = !self.name_free_assistance
            && relevant
            && (self
                .candidate_names
                .iter()
                .any(|name| conditional.unknown_contains_identifier(&include_text, name))
                || conditional.pascal_condition_contains_identifier(self.candidate_names)
                || inherited_conditional_candidate);
        let analysis = if self.bytes_read >= MAX_RENAME_INCLUDE_BYTES {
            self.stopped = true;
            IncludeAnalysis::unsafe_with_reason(
                format!(
                    "include {path:?} could not be audited because include byte limit ({MAX_RENAME_INCLUDE_BYTES}) was reached"
                ),
                relevant,
            )
        } else if let Some(reason) = nested_failure {
            IncludeAnalysis::unsafe_with_reason(reason, relevant)
        } else if !conditional.complete {
            IncludeAnalysis::unsafe_with_reason(
                format!("include {path:?} has malformed or incomplete conditional directives"),
                relevant,
            )
        } else if conditional.unknown_activity_requires_fail_closed() {
            IncludeAnalysis::unsafe_with_reason(
                format!("include {path:?} has unknown source activity"),
                relevant,
            )
        } else if conditional_candidate_is_uncertain {
            IncludeAnalysis::unsafe_with_reason(
                format!("include {path:?} has conditional compilation affecting the rename"),
                relevant,
            )
        } else if conditional.directives.iter().any(|directive| {
            directive.potentially_active() && directive.kind == ConditionalDirectiveKind::Other
        }) {
            IncludeAnalysis::unsafe_with_reason(
                format!("include {path:?} contains an unsupported directive"),
                relevant,
            )
        } else if self.name_free_assistance && relevant {
            IncludeAnalysis::unsafe_with_reason(
                format!("include {path:?} contains source content"),
                true,
            )
        } else {
            IncludeAnalysis::safe_with_environment(relevant, Some(environment))
        };
        self.cache.insert(cache_key, analysis.clone());
        Ok(analysis)
    }

    fn record_error(&mut self, error: String) {
        match self.result.errors.len().cmp(&MAX_RENAME_INCLUDE_ERRORS) {
            std::cmp::Ordering::Less => self.result.errors.push(error),
            std::cmp::Ordering::Equal => self.result.errors.push(format!(
                "rename cannot prove completeness because include error limit ({MAX_RENAME_INCLUDE_ERRORS}) was reached"
            )),
            std::cmp::Ordering::Greater => {}
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DirectiveKind {
    Include,
    ConditionalStart,
    ConditionalMiddle,
    ConditionalEnd,
    CompilerDefine,
    MethodInfo,
    Harmless,
    Other,
}

#[derive(Debug, Clone)]
struct Directive {
    kind: DirectiveKind,
    body: String,
    start: usize,
    end: usize,
}

fn legacy_directive(directive: &ConditionalDirective) -> Directive {
    let kind = match directive.kind {
        ConditionalDirectiveKind::Include => DirectiveKind::Include,
        ConditionalDirectiveKind::ConditionalStart => DirectiveKind::ConditionalStart,
        ConditionalDirectiveKind::ConditionalMiddle => DirectiveKind::ConditionalMiddle,
        ConditionalDirectiveKind::ConditionalEnd => DirectiveKind::ConditionalEnd,
        ConditionalDirectiveKind::Define | ConditionalDirectiveKind::Undef => {
            DirectiveKind::CompilerDefine
        }
        ConditionalDirectiveKind::MethodInfo => DirectiveKind::MethodInfo,
        ConditionalDirectiveKind::Harmless => DirectiveKind::Harmless,
        ConditionalDirectiveKind::Other => DirectiveKind::Other,
    };
    Directive {
        kind,
        body: directive.body.clone(),
        start: directive.start,
        end: directive.end,
    }
}

fn projected_source_contains_pascal_tokens(source: &str) -> bool {
    let bytes = source.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index].is_ascii_whitespace() {
            index += 1;
            continue;
        }
        if bytes[index] == b'/' && bytes.get(index + 1) == Some(&b'/') {
            index += 2;
            while index < bytes.len() && bytes[index] != b'\n' && bytes[index] != b'\r' {
                index += 1;
            }
            continue;
        }
        if bytes[index] == b'{' {
            let Some(close) = bytes[index + 1..].iter().position(|byte| *byte == b'}') else {
                return true;
            };
            index = index.saturating_add(close).saturating_add(2);
            continue;
        }
        if bytes[index] == b'(' && bytes.get(index + 1) == Some(&b'*') {
            let Some(close) = bytes[index + 2..]
                .windows(2)
                .position(|window| window == b"*)")
            else {
                return true;
            };
            index = index.saturating_add(close).saturating_add(4);
            continue;
        }
        return true;
    }
    false
}

fn include_search_directories(owner_path: &Path, context: Option<&ProjectContext>) -> Vec<PathBuf> {
    let mut directories = Vec::new();
    if let Some(parent) = absolute_path(owner_path.to_path_buf())
        .parent()
        .map(Path::to_path_buf)
    {
        add_include_directory(&mut directories, parent);
    }
    if let Some(context) = context {
        for path in &context.include_paths {
            add_include_directory(&mut directories, path.clone());
        }
        for path in &context.search_paths {
            add_include_directory(&mut directories, path.clone());
        }
    }
    directories
}

fn add_include_directory(directories: &mut Vec<PathBuf>, path: PathBuf) {
    let path = absolute_path(path);
    if !directories
        .iter()
        .any(|existing| path_key(existing) == path_key(&path))
    {
        directories.push(path);
    }
}

fn is_readable_include_for_context(
    workspace: &Workspace,
    path: &Path,
    context_key: &ContextKey,
    context: &ProjectContext,
    route: &IncludeRoute,
    legacy_authorized: bool,
) -> bool {
    let path = absolute_path(path.to_path_buf());
    if let IncludeRoute::Mapped { root } = route {
        return workspace.mapped_path_is_readable_under_root(&path, root, context_key);
    }
    let mapped_roots = context_key
        .overrides
        .read_roots()
        .into_iter()
        .map(|root| super::native_mapping_root(&root))
        .collect::<Vec<_>>();
    if mapped_roots
        .iter()
        .any(|root| path_starts_with_native(&path, root))
    {
        return workspace.mapped_path_is_readable(&path, context_key);
    }

    workspace.accepts_path(&path)
        || context.search_path_entries.iter().any(|entry| {
            if !path_starts_with_native(&path, &entry.path) {
                return false;
            }
            match &entry.provenance {
                ProjectPathProvenance::Mapped { root } => {
                    workspace.mapped_path_is_readable_under_root(&path, root, context_key)
                }
                ProjectPathProvenance::Configured => false,
                ProjectPathProvenance::LegacyNative => true,
            }
        })
        || context.include_path_entries.iter().any(|entry| {
            if !path_starts_with_native(&path, &entry.path) {
                return false;
            }
            match &entry.provenance {
                ProjectPathProvenance::Mapped { root } => {
                    workspace.mapped_path_is_readable_under_root(&path, root, context_key)
                }
                ProjectPathProvenance::Configured => false,
                ProjectPathProvenance::LegacyNative => true,
            }
        })
        || legacy_authorized
}

fn legacy_include_is_authorized(
    workspace: &Workspace,
    context: &ProjectContext,
    owner_path: &Path,
    selected_directory: Option<&Path>,
    relative: bool,
) -> bool {
    owner_has_legacy_or_workspace_authority(workspace, owner_path, context)
        && (!relative
            || selected_directory.is_some_and(|directory| {
                owner_path
                    .parent()
                    .is_some_and(|parent| path_key(parent) == path_key(directory))
                    || legacy_search_entry_selected(context, Some(directory))
            }))
}

fn inherited_legacy_authorization(
    inherited: bool,
    owner_path: &Path,
    directive: &Directive,
    selected_directory: Option<&Path>,
    context: &ProjectContext,
) -> bool {
    if !inherited {
        return legacy_search_entry_selected(context, selected_directory);
    }
    if !include_name(directive).is_some_and(|raw| Path::new(&raw).is_relative()) {
        return true;
    }
    selected_directory.is_some_and(|directory| {
        owner_path
            .parent()
            .is_some_and(|parent| path_key(parent) == path_key(directory))
            || legacy_search_entry_selected(context, Some(directory))
    })
}

fn legacy_search_entry_selected(
    context: &ProjectContext,
    selected_directory: Option<&Path>,
) -> bool {
    selected_directory.is_some_and(|directory| {
        context
            .search_path_entries
            .iter()
            .chain(context.include_path_entries.iter())
            .any(|entry| {
                path_key(&entry.path) == path_key(directory)
                    && matches!(entry.provenance, ProjectPathProvenance::LegacyNative)
            })
    })
}

fn include_cache_key(
    path: &Path,
    context_key: &ContextKey,
    owner_path: &Path,
    legacy_route: Option<&LegacyRoute>,
    environment: Option<&conditional::ConditionalEnvironment>,
) -> String {
    let mut hasher = DefaultHasher::new();
    path_key(path).hash(&mut hasher);
    context_key.hash(&mut hasher);
    path_key(owner_path).hash(&mut hasher);
    legacy_route.hash(&mut hasher);
    environment
        .map(conditional::ConditionalEnvironment::fingerprint)
        .hash(&mut hasher);
    format!("include:{:016x}", hasher.finish())
}

fn include_resolution_cache_key(
    context_key: &ContextKey,
    including_path: &Path,
    requested_name: &str,
    legacy_route: Option<&LegacyRoute>,
) -> String {
    let mut hasher = DefaultHasher::new();
    context_key.hash(&mut hasher);
    path_key(including_path).hash(&mut hasher);
    requested_name.hash(&mut hasher);
    legacy_route.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

fn owner_has_legacy_or_workspace_authority(
    workspace: &Workspace,
    owner_path: &Path,
    context: &ProjectContext,
) -> bool {
    if workspace.accepts_path(owner_path) {
        return true;
    }
    let is_legacy = |entry: &pascal_project::ProjectPathEntry| {
        matches!(entry.provenance, ProjectPathProvenance::LegacyNative)
            && path_starts_with_native(owner_path, &entry.path)
    };
    context.search_path_entries.iter().any(is_legacy)
        || context.include_path_entries.iter().any(is_legacy)
        || context.main_source_entry.as_ref().is_some_and(is_legacy)
        || context
            .explicit_unit_entries
            .values()
            .flatten()
            .any(is_legacy)
}

fn canonical_include_key(path: &Path) -> String {
    fs::canonicalize(path)
        .map(|canonical| path_key(&canonical))
        .unwrap_or_else(|_| path_key(path))
}

#[cfg(test)]
#[allow(dead_code)]
fn resolve_include_path(directive: &Directive, directories: &[PathBuf]) -> IncludeLookup {
    resolve_include_path_with_overrides(directive, directories, &EffectiveOverrides::default())
}

#[allow(dead_code)]
fn resolve_include_path_with_overrides(
    directive: &Directive,
    directories: &[PathBuf],
    overrides: &EffectiveOverrides,
) -> IncludeLookup {
    resolve_include_path_with_overrides_and_overlay(directive, directories, overrides, |_| false)
}

fn resolve_include_path_with_overrides_and_overlay(
    directive: &Directive,
    directories: &[PathBuf],
    overrides: &EffectiveOverrides,
    has_overlay: impl Fn(&Path) -> bool,
) -> IncludeLookup {
    let Some(raw) = include_name(directive) else {
        return IncludeLookup {
            observations: Vec::new(),
            selected: None,
            selected_directory: None,
            selected_route: IncludeRoute::Legacy,
            error: None,
        };
    };

    let mut observations = Vec::new();
    let mut selected = None;
    let mut selected_directory = None;
    let mut selected_route = IncludeRoute::Legacy;
    let mut error = None;
    for directory in directories {
        observations.push(IncludeObservation {
            path: directory.clone(),
            stamp: path_stamp(directory),
        });
        let (candidate, route) = match overrides.resolve_path(&raw, directory) {
            Ok(resolved) => {
                let route = resolved
                    .mapping
                    .as_ref()
                    .map_or(IncludeRoute::Legacy, |mapping| IncludeRoute::Mapped {
                        root: super::native_mapping_root(&mapping.to),
                    });
                (absolute_path(resolved.path), route)
            }
            Err(resolve_error) => {
                error = Some(resolve_error);
                break;
            }
        };
        if observations
            .iter()
            .any(|observation| path_key(&observation.path) == path_key(&candidate))
        {
            continue;
        }
        let metadata = fs::metadata(&candidate);
        let stamp = path_stamp(&candidate);
        observations.push(IncludeObservation {
            path: candidate.clone(),
            stamp,
        });
        if has_overlay(&candidate) {
            selected = Some(candidate);
            selected_directory = Some(directory.clone());
            selected_route = route;
            break;
        }
        match metadata {
            Ok(metadata) if metadata.is_file() => {
                selected = Some(candidate);
                selected_directory = Some(directory.clone());
                selected_route = route;
                break;
            }
            Ok(_) => {}
            Err(io_error) if io_error.kind() == std::io::ErrorKind::NotFound => {
                let case_lookup = resolve_case_insensitive_include_path(&candidate);
                observations.extend(case_lookup.observations);
                if let Some(case_error) = case_lookup.error {
                    error = Some(case_error);
                    break;
                }
                if let Some(actual) = case_lookup.selected {
                    let actual_metadata = fs::metadata(&actual);
                    observations.push(IncludeObservation {
                        path: actual.clone(),
                        stamp: path_stamp(&actual),
                    });
                    match actual_metadata {
                        Ok(metadata) if metadata.is_file() => {
                            selected = Some(actual);
                            selected_directory = Some(directory.clone());
                            selected_route = route;
                            break;
                        }
                        Ok(_) => {}
                        Err(io_error) if io_error.kind() == std::io::ErrorKind::NotFound => {}
                        Err(io_error) => {
                            error = Some(format!("{}: {io_error}", actual.display()));
                            break;
                        }
                    }
                }
            }
            Err(io_error) => {
                error = Some(format!("{}: {io_error}", candidate.display()));
                break;
            }
        }
    }
    IncludeLookup {
        observations,
        selected,
        selected_directory,
        selected_route,
        error,
    }
}

#[derive(Debug)]
struct CaseInsensitiveIncludeLookup {
    observations: Vec<IncludeObservation>,
    selected: Option<PathBuf>,
    error: Option<String>,
}

fn resolve_case_insensitive_include_path(path: &Path) -> CaseInsensitiveIncludeLookup {
    let absolute = absolute_path(path.to_path_buf());
    let mut base = absolute.clone();
    while !base.exists() {
        if !base.pop() {
            base = PathBuf::from(std::path::MAIN_SEPARATOR.to_string());
            break;
        }
    }
    let relative = absolute
        .strip_prefix(&base)
        .unwrap_or_else(|_| Path::new(""));
    let mut current = base.clone();
    let mut observations = Vec::new();
    observations.push(IncludeObservation {
        path: current.clone(),
        stamp: path_stamp(&current),
    });
    for component in relative.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                let _ = current.pop();
            }
            Component::Prefix(prefix) => current.push(prefix.as_os_str()),
            Component::RootDir => current.push(std::path::MAIN_SEPARATOR.to_string()),
            Component::Normal(component) => {
                let wanted = component.to_string_lossy();
                let entries = match fs::read_dir(&current) {
                    Ok(entries) => entries,
                    Err(error) => {
                        return CaseInsensitiveIncludeLookup {
                            observations,
                            selected: None,
                            error: Some(format!(
                                "could not inspect {} while resolving case-insensitive include: {error}",
                                current.display()
                            )),
                        };
                    }
                };
                let mut matches = entries
                    .filter_map(Result::ok)
                    .filter_map(|entry| {
                        entry
                            .file_name()
                            .to_string_lossy()
                            .eq_ignore_ascii_case(&wanted)
                            .then_some(entry.path())
                    })
                    .collect::<Vec<_>>();
                if matches.len() > 1 {
                    return CaseInsensitiveIncludeLookup {
                        observations,
                        selected: None,
                        error: Some(format!(
                            "ambiguous case-insensitive include path component {wanted:?} under {}",
                            current.display()
                        )),
                    };
                }
                let Some(next) = matches.pop() else {
                    observations.push(IncludeObservation {
                        path: current.join(component),
                        stamp: None,
                    });
                    return CaseInsensitiveIncludeLookup {
                        observations,
                        selected: None,
                        error: None,
                    };
                };
                let stamp = match next.metadata() {
                    Ok(_) => path_stamp(&next),
                    Err(error) => {
                        observations.push(IncludeObservation {
                            path: next.clone(),
                            stamp: None,
                        });
                        return CaseInsensitiveIncludeLookup {
                            observations,
                            selected: None,
                            error: Some(format!(
                                "could not inspect {} while resolving case-insensitive include: {error}",
                                next.display()
                            )),
                        };
                    }
                };
                observations.push(IncludeObservation {
                    path: next.clone(),
                    stamp,
                });
                current = next;
            }
        }
    }
    CaseInsensitiveIncludeLookup {
        observations,
        selected: Some(current),
        error: None,
    }
}

fn include_name(directive: &Directive) -> Option<String> {
    let raw = directive
        .body
        .trim_start()
        .split_once(|character: char| character.is_ascii_whitespace() || character == ':')
        .map(|(_, remainder)| remainder.trim())
        .filter(|remainder| !remainder.is_empty())?;
    if raw.is_empty() || raw.contains("$(") {
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

fn include_owner_summary(source: &str, owner_directives: &[Directive]) -> Option<String> {
    let mut summary = String::new();
    for directive in owner_directives {
        let fragment = source.get(directive.start..directive.end)?;
        if summary
            .len()
            .saturating_add(fragment.len())
            .saturating_add(1)
            > MAX_RENAME_INCLUDE_OWNER_SUMMARY_BYTES
        {
            return None;
        }
        if !summary.is_empty() {
            summary.push('\n');
        }
        summary.push_str(fragment);
    }
    (!owner_directives.is_empty() && !summary.is_empty()).then_some(summary)
}

fn directive_keyword(body: &str) -> Option<&str> {
    body.trim_start()
        .split(|character: char| character.is_ascii_whitespace() || character == ':')
        .next()
        .filter(|keyword| !keyword.is_empty())
}

#[derive(Debug, Clone)]
struct IncludeSource {
    text: String,
    content_hash: u64,
    #[allow(dead_code)]
    bytes: usize,
}

pub(super) fn expand_source_with_workspace(
    workspace: &Workspace,
    root_uri: &Url,
    source: &str,
    context_key: &ContextKey,
    limits: ExpansionLimits,
    cancel: &AtomicBool,
) -> Result<ExpansionResult, String> {
    let context = workspace
        .contexts
        .get(context_key)
        .map(|state| state.context.clone())
        .ok_or_else(|| format!("project context was not retained for {root_uri}"))?;
    let mut resolver = WorkspaceIncludeResolver {
        workspace,
        context: &context,
        context_key,
        max_file_bytes: workspace.options.limits.max_file_bytes,
        max_total_bytes: workspace.options.limits.max_total_bytes,
        legacy_authorizations: HashMap::new(),
    };
    let conditional_context = context.effective_conditional_context();
    include_expansion::expand_source_with_context(
        root_uri.clone(),
        source,
        &conditional_context,
        &mut resolver,
        limits,
        cancel,
    )
}

struct WorkspaceIncludeResolver<'a> {
    workspace: &'a Workspace,
    context: &'a ProjectContext,
    context_key: &'a ContextKey,
    max_file_bytes: usize,
    max_total_bytes: usize,
    legacy_authorizations: HashMap<Url, bool>,
}

impl IncludeResolver for WorkspaceIncludeResolver<'_> {
    fn resolve_include(
        &mut self,
        owner: &Url,
        body: &str,
        cancel: &AtomicBool,
    ) -> Result<ResolvedInclude, String> {
        if is_cancelled(cancel) {
            return Err(CANCELLATION_MESSAGE.to_string());
        }
        let directive = Directive {
            kind: DirectiveKind::Include,
            body: body.to_owned(),
            start: 0,
            end: body.len(),
        };
        let owner_path = owner
            .to_file_path()
            .map(absolute_path)
            .map_err(|_| format!("include owner is not a file URI: {owner}"))?;
        let directories = include_search_directories(&owner_path, Some(self.context));
        let lookup = resolve_include_path_with_overrides_and_overlay(
            &directive,
            &directories,
            &self.context.overrides,
            |candidate| {
                Url::from_file_path(candidate)
                    .ok()
                    .map(|uri| super::canonical_file_uri(&uri))
                    .and_then(|uri| self.workspace.open_documents.get(&uri))
                    .is_some_and(|document| document.text.is_some())
            },
        );
        if let Some(error) = lookup.error.as_ref() {
            return Err(error.clone());
        }
        let path = lookup
            .selected
            .as_ref()
            .cloned()
            .ok_or_else(|| "include path is unresolved".to_string())?;
        let route = lookup.selected_route.clone();
        let relative = include_name(&directive).is_some_and(|raw| Path::new(&raw).is_relative());
        let legacy_authorized = if !matches!(&route, IncludeRoute::Legacy) {
            false
        } else if let Some(inherited) = self.legacy_authorizations.get(owner).copied() {
            inherited_legacy_authorization(
                inherited,
                &owner_path,
                &directive,
                lookup.selected_directory.as_deref(),
                self.context,
            )
        } else {
            legacy_include_is_authorized(
                self.workspace,
                self.context,
                &owner_path,
                lookup.selected_directory.as_deref(),
                relative,
            )
        };
        if !is_readable_include_for_context(
            self.workspace,
            &path,
            self.context_key,
            self.context,
            &route,
            legacy_authorized,
        ) {
            return Err(format!(
                "include {path:?} is outside the owning project's readable roots"
            ));
        }
        let path_entry = match &route {
            IncludeRoute::Mapped { root } => ProjectPathEntry {
                path: path.clone(),
                provenance: ProjectPathProvenance::Mapped { root: root.clone() },
            },
            IncludeRoute::Legacy => super::context_path_entry(self.context, &path)
                .or_else(|| {
                    legacy_authorized.then(|| ProjectPathEntry {
                        path: path.clone(),
                        provenance: ProjectPathProvenance::LegacyNative,
                    })
                })
                .ok_or_else(|| "include has no requester-scoped read authorization".to_string())?,
        };
        let include_uri = Url::from_file_path(&path)
            .map(|uri| super::canonical_file_uri(&uri))
            .map_err(|()| format!("could not create a URI for include {path:?}"))?;
        let mut observations = self.expansion_observations(&lookup);
        self.legacy_authorizations
            .insert(include_uri.clone(), legacy_authorized);
        if let Some(document) = self.workspace.open_documents.get(&include_uri) {
            if let Some(reason) = &document.rejection {
                return Err(format!("include {include_uri} was rejected: {reason}"));
            }
            if let Some(text) = &document.text {
                return Ok(ResolvedInclude {
                    uri: include_uri,
                    text: text.clone(),
                    path_entry: Some(path_entry),
                    observations,
                });
            }
        }
        let source = match &route {
            IncludeRoute::Mapped { .. } => read_mapped_include(
                &self.context.read_policy,
                &path_entry,
                self.max_file_bytes,
                self.max_total_bytes,
                cancel,
            )?,
            IncludeRoute::Legacy => read_include(
                &path,
                &self.context.read_policy,
                &path_entry,
                self.max_file_bytes,
                Some(self.max_total_bytes),
                Some(cancel),
            )?,
        };
        if let Some(observation) = observations
            .iter_mut()
            .find(|observation| paths_equal_ci(&observation.path, &path))
        {
            observation.content_hash = Some(source.content_hash);
            observation.present = true;
        }
        Ok(ResolvedInclude {
            uri: include_uri,
            text: source.text,
            path_entry: Some(path_entry),
            observations,
        })
    }

    fn include_size_hint(
        &mut self,
        owner: &Url,
        body: &str,
        cancel: &AtomicBool,
    ) -> Result<Option<usize>, String> {
        if is_cancelled(cancel) {
            return Err(CANCELLATION_MESSAGE.to_string());
        }
        let directive = Directive {
            kind: DirectiveKind::Include,
            body: body.to_owned(),
            start: 0,
            end: body.len(),
        };
        let owner_path = owner
            .to_file_path()
            .map(absolute_path)
            .map_err(|_| format!("include owner is not a file URI: {owner}"))?;
        let directories = include_search_directories(&owner_path, Some(self.context));
        let lookup = resolve_include_path_with_overrides_and_overlay(
            &directive,
            &directories,
            &self.context.overrides,
            |candidate| {
                Url::from_file_path(candidate)
                    .ok()
                    .map(|uri| super::canonical_file_uri(&uri))
                    .and_then(|uri| self.workspace.open_documents.get(&uri))
                    .is_some_and(|document| document.text.is_some())
            },
        );
        if let Some(error) = lookup.error {
            return Err(error);
        }
        let Some(path) = lookup.selected else {
            return Ok(None);
        };
        let Some(uri) = Url::from_file_path(&path)
            .ok()
            .map(|uri| super::canonical_file_uri(&uri))
        else {
            return Ok(None);
        };
        if let Some(text) = self
            .workspace
            .open_documents
            .get(&uri)
            .and_then(|document| document.text.as_ref())
        {
            return Ok(Some(text.len()));
        }
        Ok(fs::metadata(path)
            .ok()
            .filter(|metadata| metadata.is_file())
            .and_then(|metadata| usize::try_from(metadata.len()).ok()))
    }
}

impl WorkspaceIncludeResolver<'_> {
    fn expansion_observations(&self, lookup: &IncludeLookup) -> Vec<ExpansionIncludeObservation> {
        lookup
            .observations
            .iter()
            .map(|observation| {
                let overlay = Url::from_file_path(&observation.path)
                    .ok()
                    .map(|uri| super::canonical_file_uri(&uri))
                    .and_then(|uri| self.workspace.open_documents.get(&uri))
                    .and_then(|document| {
                        document.text.as_ref().map(|text| (document.version, text))
                    });
                ExpansionIncludeObservation {
                    path: observation.path.clone(),
                    stamp: observation.stamp.clone(),
                    present: observation.stamp.is_some() || overlay.is_some(),
                    overlay_version: overlay.map(|(version, _)| version),
                    content_hash: overlay.map(|(_, text)| text_content_hash(text)),
                }
            })
            .collect()
    }
}

fn read_include(
    path: &Path,
    read_policy: &ReadPolicy,
    path_entry: &ProjectPathEntry,
    max_file_bytes: usize,
    max_total_bytes: Option<usize>,
    cancel: Option<&AtomicBool>,
) -> Result<IncludeSource, String> {
    if cancel.is_some_and(is_cancelled) {
        return Err(CANCELLATION_MESSAGE.to_string());
    }
    let legacy_payload = matches!(path_entry.provenance, ProjectPathProvenance::LegacyNative);
    if if legacy_payload {
        !read_policy.allows_legacy_payload_entry(path_entry)
    } else {
        !read_policy.allows_entry(path_entry)
    } {
        return Err("payload path is not authorized".to_string());
    }
    let metadata = fs::metadata(path).map_err(|error| error.to_string())?;
    if !metadata.is_file() {
        return Err("path is not a regular file".to_string());
    }
    if metadata.len() > max_file_bytes as u64 {
        return Err(format!(
            "file exceeds the configured per-file limit {max_file_bytes}"
        ));
    }
    if max_total_bytes.is_some_and(|limit| metadata.len() > limit as u64) {
        return Err(INCLUDE_BYTE_BUDGET_ERROR.to_string());
    }
    if cancel.is_some_and(is_cancelled) {
        return Err(CANCELLATION_MESSAGE.to_string());
    }
    let limit = max_total_bytes.map_or(max_file_bytes, |total| max_file_bytes.min(total));
    let bytes = if legacy_payload {
        read_policy.read_legacy_payload_bytes(path_entry, limit as u64)
    } else {
        read_policy.read_payload_bytes(path_entry, limit as u64)
    }
    .map_err(|error| error.to_string())?;
    let byte_count = bytes.len();
    if cancel.is_some_and(is_cancelled) {
        return Err(CANCELLATION_MESSAGE.to_string());
    }
    if let Some(encoding) = unsupported_source_encoding(&bytes) {
        return Err(format!("unsupported {encoding} source encoding"));
    }
    Ok(IncludeSource {
        text: decode_bytes(&bytes).into_owned(),
        content_hash: super::content_hash_bytes(&bytes),
        bytes: byte_count,
    })
}

fn read_mapped_include(
    read_policy: &ReadPolicy,
    path_entry: &ProjectPathEntry,
    max_file_bytes: usize,
    max_total_bytes: usize,
    cancel: &AtomicBool,
) -> Result<IncludeSource, String> {
    if is_cancelled(cancel) {
        return Err(CANCELLATION_MESSAGE.to_string());
    }
    let limit = max_file_bytes.min(max_total_bytes);
    let bytes = read_policy.read_payload_bytes(path_entry, limit as u64)?;
    if is_cancelled(cancel) {
        return Err(CANCELLATION_MESSAGE.to_string());
    }
    if let Some(encoding) = unsupported_source_encoding(&bytes) {
        return Err(format!("unsupported {encoding} source encoding"));
    }
    Ok(IncludeSource {
        text: decode_bytes(&bytes).into_owned(),
        content_hash: super::content_hash_bytes(&bytes),
        bytes: bytes.len(),
    })
}

fn read_record_content_bytes(
    path: &Path,
    record: &SourceRecord,
    cancel: &AtomicBool,
) -> Result<Vec<u8>, String> {
    if record.include_payload {
        return Err(
            "include payload does not have a byte-for-byte revalidation record".to_string(),
        );
    }
    if is_configuration_file(path) && record.read_policy.is_none() && record.path_entry.is_none() {
        return read_configuration_record_bytes(path, cancel);
    }
    let (read_policy, path_entry) = record.payload_dependency()?;
    read_exact_file_bytes(path, read_policy, path_entry, cancel)
}

fn read_record_content_hash(
    path: &Path,
    record: &SourceRecord,
    cancel: &AtomicBool,
) -> Result<u64, String> {
    if record.include_payload {
        let (read_policy, path_entry) = record.payload_dependency()?;
        let bytes = if matches!(path_entry.provenance, ProjectPathProvenance::LegacyNative) {
            read_policy.read_legacy_payload_bytes(path_entry, MAX_RENAME_SCAN_FILE_BYTES as u64)?
        } else {
            read_policy.read_payload_bytes(path_entry, MAX_RENAME_SCAN_FILE_BYTES as u64)?
        };
        if is_cancelled(cancel) {
            return Err(CANCELLATION_MESSAGE.to_string());
        }
        return Ok(super::content_hash_bytes(&bytes));
    }
    if is_configuration_file(path) && record.read_policy.is_none() && record.path_entry.is_none() {
        let bytes = read_configuration_record_bytes(path, cancel)?;
        return Ok(super::content_hash_bytes(&bytes));
    }
    let (read_policy, path_entry) = record.payload_dependency()?;
    file_content_hash(path, read_policy, path_entry, cancel)
}

fn read_configuration_record_bytes(path: &Path, cancel: &AtomicBool) -> Result<Vec<u8>, String> {
    if is_cancelled(cancel) {
        return Err(CANCELLATION_MESSAGE.to_string());
    }
    let link_metadata = fs::symlink_metadata(path).map_err(|error| {
        format!(
            "could not inspect configuration candidate {}: {error}",
            path.display()
        )
    })?;
    let metadata = if link_metadata.file_type().is_symlink() {
        fs::metadata(path).map_err(|error| {
            format!(
                "could not inspect configuration candidate {}: {error}",
                path.display()
            )
        })?
    } else {
        link_metadata
    };
    if !metadata.is_file() {
        return Err(format!(
            "configuration candidate {} is not a regular file",
            path.display()
        ));
    }
    let file = open_configuration_record(path).map_err(|error| {
        format!(
            "could not read configuration candidate {}: {error}",
            path.display()
        )
    })?;
    let mut bytes = Vec::new();
    file.take((MAX_RENAME_CONFIG_BYTES as u64).saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|error| {
            format!(
                "could not read configuration candidate {}: {error}",
                path.display()
            )
        })?;
    if bytes.len() > MAX_RENAME_CONFIG_BYTES {
        return Err(format!(
            "configuration candidate {} exceeds the maximum size of {MAX_RENAME_CONFIG_BYTES} bytes",
            path.display()
        ));
    }
    if is_cancelled(cancel) {
        return Err(CANCELLATION_MESSAGE.to_string());
    }
    Ok(bytes)
}

#[cfg(target_os = "linux")]
fn open_configuration_record(path: &Path) -> io::Result<fs::File> {
    use std::fs::OpenOptions;
    use std::os::unix::fs::OpenOptionsExt;

    const O_NONBLOCK: i32 = 0o4000;
    OpenOptions::new()
        .read(true)
        .custom_flags(O_NONBLOCK)
        .open(path)
}

#[cfg(not(target_os = "linux"))]
fn open_configuration_record(path: &Path) -> io::Result<fs::File> {
    fs::File::open(path)
}

#[allow(dead_code)]
fn directive_only_directives(source: &str, allow_includes: bool) -> Option<Vec<Directive>> {
    let bytes = source.as_bytes();
    let mut entries = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index].is_ascii_whitespace() {
            index += 1;
            continue;
        }
        if bytes[index] == b'/' && bytes.get(index + 1) == Some(&b'/') {
            index = source[index..]
                .find('\n')
                .map_or(bytes.len(), |offset| index + offset + 1);
            continue;
        }
        if bytes[index] == b'{' {
            if bytes.get(index + 1) == Some(&b'$') {
                let end = source[index + 2..].find('}')?;
                let body = source[index + 2..index + 2 + end].to_string();
                entries.push(Directive {
                    kind: directive_kind(&body),
                    body,
                    start: index,
                    end: index + end + 3,
                });
                index += end + 3;
            } else {
                let end = source[index + 1..].find('}')?;
                index += end + 2;
            }
            continue;
        }
        if bytes[index] == b'(' && bytes.get(index + 1) == Some(&b'*') {
            if bytes.get(index + 2) == Some(&b'$') {
                let end = source[index + 3..].find("*)")?;
                let body = source[index + 3..index + 3 + end].to_string();
                entries.push(Directive {
                    kind: directive_kind(&body),
                    body,
                    start: index,
                    end: index + end + 5,
                });
                index += end + 5;
            } else {
                let end = source[index + 2..].find("*)")?;
                index += end + 4;
            }
            continue;
        }
        return None;
    }

    let mut conditional_frames = Vec::new();
    for directive in &entries {
        match directive.kind {
            DirectiveKind::ConditionalStart => conditional_frames.push(false),
            DirectiveKind::ConditionalMiddle => {
                let seen_else = conditional_frames.last_mut()?;
                if directive_keyword(&directive.body)
                    .is_some_and(|keyword| keyword.eq_ignore_ascii_case("else"))
                {
                    if *seen_else {
                        return None;
                    }
                    *seen_else = true;
                } else if *seen_else {
                    return None;
                }
            }
            DirectiveKind::ConditionalEnd => {
                conditional_frames.pop()?;
            }
            DirectiveKind::Include if allow_includes => {}
            DirectiveKind::CompilerDefine | DirectiveKind::MethodInfo | DirectiveKind::Harmless => {
            }
            DirectiveKind::Include | DirectiveKind::Other => return None,
        }
    }
    conditional_frames.is_empty().then_some(entries)
}

#[allow(dead_code)]
fn conditional_regions(source: &str) -> Vec<(usize, usize)> {
    let mut starts = Vec::new();
    let mut regions = Vec::new();
    for directive in directives(source) {
        match directive.kind {
            DirectiveKind::ConditionalStart => starts.push(directive.start),
            DirectiveKind::ConditionalEnd => {
                if let Some(start) = starts.pop() {
                    regions.push((start, directive.end));
                }
            }
            _ => {}
        }
    }
    regions.extend(starts.into_iter().map(|start| (start, source.len())));
    regions
}

pub(crate) fn identifier_at_position(source: &str, position: Position) -> Option<String> {
    let offset = text::position_to_offset(source, position)?;
    let mut start = offset.min(source.len());
    if start == source.len() || !is_identifier_byte(source.as_bytes()[start]) {
        start = start.saturating_sub(1);
    }
    while start > 0 && is_identifier_byte(source.as_bytes()[start - 1]) {
        start -= 1;
    }
    let mut end = start;
    while end < source.len() && is_identifier_byte(source.as_bytes()[end]) {
        end += 1;
    }
    source
        .get(start..end)
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
}

fn directives(source: &str) -> Vec<Directive> {
    let bytes = source.as_bytes();
    let mut directives = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'\'' => skip_string(bytes, &mut index),
            b'/' if bytes.get(index + 1) == Some(&b'/') => {
                index = source[index..]
                    .find('\n')
                    .map_or(bytes.len(), |offset| index + offset + 1);
            }
            b'{' => {
                if bytes.get(index + 1) == Some(&b'$') {
                    let Some(end) = source[index + 2..].find('}') else {
                        break;
                    };
                    let body = source[index + 2..index + 2 + end].to_string();
                    directives.push(Directive {
                        kind: directive_kind(&body),
                        body,
                        start: index,
                        end: index + end + 3,
                    });
                    index += end + 3;
                } else {
                    index = source[index + 1..]
                        .find('}')
                        .map_or(bytes.len(), |offset| index + offset + 2);
                }
            }
            b'(' if bytes.get(index + 1) == Some(&b'*') => {
                if bytes.get(index + 2) == Some(&b'$') {
                    let Some(end) = source[index + 3..].find("*)") else {
                        break;
                    };
                    let body = source[index + 3..index + 3 + end].to_string();
                    directives.push(Directive {
                        kind: directive_kind(&body),
                        body,
                        start: index,
                        end: index + end + 5,
                    });
                    index += end + 5;
                } else {
                    let Some(end) = source[index + 2..].find("*)") else {
                        break;
                    };
                    index += end + 4;
                }
            }
            _ => index += 1,
        }
    }
    directives
}

fn directive_kind(body: &str) -> DirectiveKind {
    let Some(keyword) = directive_keyword(body) else {
        return DirectiveKind::Other;
    };
    let keyword = keyword.to_ascii_lowercase();
    if keyword == "i" || keyword == "include" {
        return DirectiveKind::Include;
    }
    if keyword == "endif" || keyword == "ifend" {
        return DirectiveKind::ConditionalEnd;
    }
    if keyword == "else" || keyword == "elseif" || keyword == "elif" {
        return DirectiveKind::ConditionalMiddle;
    }
    if matches!(keyword.as_str(), "if" | "ifdef" | "ifndef" | "ifopt") {
        return DirectiveKind::ConditionalStart;
    }
    if keyword == "define" || keyword == "undef" {
        return DirectiveKind::CompilerDefine;
    }
    if keyword == "methodinfo" {
        return DirectiveKind::MethodInfo;
    }
    if is_harmless_directive(body) {
        return DirectiveKind::Harmless;
    }
    DirectiveKind::Other
}

fn is_harmless_directive(body: &str) -> bool {
    if body.contains(',') {
        split_directive_parts(body)
            .into_iter()
            .all(|part| directive_keyword(part).is_some_and(is_harmless_directive_keyword))
    } else {
        directive_keyword(body).is_some_and(is_harmless_directive_keyword)
    }
}

fn split_directive_parts(body: &str) -> Vec<&str> {
    let bytes = body.as_bytes();
    let mut parts = Vec::new();
    let mut start = 0;
    let mut quote = None;
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        if let Some(quote_byte) = quote {
            if byte == quote_byte {
                if bytes.get(index + 1) == Some(&quote_byte) {
                    index += 2;
                    continue;
                }
                quote = None;
            }
        } else if byte == b'\'' || byte == b'"' {
            quote = Some(byte);
        } else if byte == b',' {
            parts.push(&body[start..index]);
            start = index + 1;
        }
        index += 1;
    }
    parts.push(&body[start..]);
    parts
}

fn is_harmless_directive_keyword(keyword: &str) -> bool {
    let keyword = keyword
        .trim()
        .trim_end_matches(['+', '-'])
        .to_ascii_lowercase();
    matches!(
        keyword.as_str(),
        "apptype"
            | "asmmode"
            | "assertions"
            | "booleval"
            | "debug"
            | "debugsymbols"
            | "endregion"
            | "excessprecision"
            | "extendedsyntax"
            | "h"
            | "hints"
            | "longstrings"
            | "m"
            | "message"
            | "mode"
            | "objexportall"
            | "optimization"
            | "overflowchecks"
            | "q"
            | "r"
            | "rangechecks"
            | "region"
            | "rtti"
            | "stronglinktypes"
            | "t"
            | "typedaddress"
            | "warn"
            | "warnings"
            | "writeableconst"
            | "x"
    )
}

fn skip_string(bytes: &[u8], index: &mut usize) {
    *index += 1;
    while *index < bytes.len() {
        if bytes[*index] != b'\'' {
            *index += 1;
        } else if bytes.get(*index + 1) == Some(&b'\'') {
            *index += 2;
        } else {
            *index += 1;
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::FileChange;
    use super::super::MetadataObservation;
    use super::{
        BaselineAccumulator, ContextKey, ContextState, DirectiveKind, Enumeration, PathStamp,
        ProjectCandidateMembership, ProjectContext, ProjectPathEntry, ProjectPathProvenance,
        ReadPolicy, SnapshotMode, Workspace, WorkspaceOptions, build_snapshot,
        capture_consumed_configuration_baseline, capture_context_baseline, contains_any_identifier,
        directive_kind, enumerate_external_overlays, file_content_hash,
        install_snapshot_priority_barrier, path_key, path_record_at, read_exact_file_bytes,
        read_record_content_hash, rename_from_input, revalidate_input, snapshot_records,
        test_cancel_in_include_analysis,
    };
    use lsp_types::{Position, Url};
    use pascal_core::resolver::{
        ResolutionObservation, ResolutionReport, SourceId, SourceRevision,
    };
    use pascal_project::delphi_overrides::{EffectiveOverrides, OverrideSession};
    use std::collections::{HashMap, HashSet};
    use std::fs::{self, File, FileTimes};
    #[cfg(unix)]
    use std::os::unix::fs::symlink;
    use std::path::PathBuf;
    #[cfg(target_os = "linux")]
    use std::process::Command;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    use std::sync::mpsc;
    use std::thread;
    #[cfg(target_os = "linux")]
    use std::time::{Duration, Instant};

    fn test_workspace(roots: Vec<PathBuf>, options: WorkspaceOptions) -> Workspace {
        Workspace::with_override_session(roots, options, OverrideSession::new(None))
    }

    #[test]
    fn local_snapshot_reuses_an_unchanged_document_model() {
        let root = tempfile::tempdir().expect("workspace root");
        let uri = Url::from_file_path(root.path().join("Snapshot.pas")).expect("fixture URI");
        let source = "unit Snapshot;\ninterface\nimplementation\nend.\n";
        let mut workspace = test_workspace(vec![root.path().to_path_buf()], Default::default());
        workspace
            .open_document(uri.clone(), source.to_owned(), 1)
            .expect("open document");
        let input = workspace.analysis_input();
        let cached = input
            .cached_documents
            .get(&uri)
            .expect("indexed document should be available to snapshots")
            .parsed
            .clone();
        let cancel = AtomicBool::new(false);

        let snapshot = build_snapshot(
            &input,
            std::slice::from_ref(&uri),
            &[],
            SnapshotMode::Local,
            None,
            &[],
            &cancel,
        )
        .expect("local snapshot");
        let snapshot_document = snapshot
            .index
            .reusable_documents()
            .into_iter()
            .find(|(document_uri, _)| document_uri == &uri)
            .map(|(_, parsed)| parsed)
            .expect("snapshot document");

        assert!(
            Arc::ptr_eq(&cached, &snapshot_document),
            "unchanged snapshot input should reuse the immutable parsed model"
        );
    }

    #[test]
    fn repeated_include_occurrences_share_one_payload_load() {
        let temp = tempfile::tempdir().expect("temporary workspace");
        let root = temp.path().to_path_buf();
        let main = root.join("Main.pas");
        let include = root.join("body.inc");
        fs::write(
            &main,
            "unit Main;\ninterface\nconst BadConst = 1;\nimplementation\n{$I body.inc}\n{$I body.inc}\n{$I body.inc}\nend.\n",
        )
        .expect("main source");
        fs::write(&include, "{ whitespace only }\n").expect("include source");

        let workspace = test_workspace(vec![root.clone()], WorkspaceOptions::default());
        let input = workspace.analysis_input();
        let uri = Url::from_file_path(&main).expect("main URI");
        let cancel = AtomicBool::new(false);
        super::super::resolver::reset_test_source_loads();
        let computed = rename_from_input(
            input,
            &uri,
            Position::new(2, 6),
            "GoodConst",
            false,
            &cancel,
        );

        assert!(
            computed.value.is_ok(),
            "rename result: {:?}",
            computed.value
        );
        assert_eq!(
            super::super::resolver::test_source_load_count(&include),
            1,
            "repeated include occurrences must reuse one resolver payload"
        );
    }

    #[test]
    fn discarded_payload_observation_revalidates_equal_stamp_mutation() {
        let temp = tempfile::tempdir().expect("temporary workspace");
        let root = temp.path().to_path_buf();
        let candidate = root.join("Provider.pas");
        let original = b"unit Wrong; interface implementation end.\n";
        let changed = b"unit Right; interface implementation end.\n";
        assert_eq!(original.len(), changed.len());
        fs::write(&candidate, original).expect("candidate");
        let root_entry = ProjectPathEntry {
            path: root.clone(),
            provenance: ProjectPathProvenance::Configured,
        };
        let candidate_entry = ProjectPathEntry {
            path: candidate.clone(),
            provenance: ProjectPathProvenance::Configured,
        };
        let context = ProjectContext {
            discovery_complete: true,
            search_paths: vec![root.clone()],
            search_path_entries: vec![root_entry],
            read_policy: ReadPolicy::new(
                std::slice::from_ref(&root),
                &[],
                &[],
                &EffectiveOverrides::default(),
            ),
            ..ProjectContext::default()
        };
        let stamp = super::path_stamp_result(&candidate)
            .expect("candidate stamp")
            .expect("candidate exists");
        let report = ResolutionReport {
            observations: vec![
                ResolutionObservation::Candidate {
                    path: candidate.clone(),
                    entry: Some(candidate_entry.clone()),
                    stamp: Some(stamp.clone()),
                    present: true,
                },
                ResolutionObservation::Payload {
                    source_id: SourceId::new(format!("source:{}", candidate.display())),
                    path: candidate.clone(),
                    revision: SourceRevision::Disk {
                        stamp: stamp.clone(),
                        content_hash: pascal_project::content_hash_bytes(original),
                        read_policy: context.read_policy.clone(),
                        path_entry: candidate_entry,
                    },
                },
            ],
            warnings: Vec::new(),
            complete: true,
            incomplete_reasons: Vec::new(),
        };
        let record = super::super::resolver::report_records(&context, &report)
            .into_iter()
            .find(|record| record.path.as_deref() == Some(candidate.as_path()))
            .expect("discarded candidate payload record");
        assert_eq!(
            record.content_hash,
            Some(pascal_project::content_hash_bytes(original))
        );
        assert!(record.read_policy.is_some());
        assert!(record.path_entry.is_some());
        let cancel = AtomicBool::new(false);
        super::revalidate_path_record(&candidate, &record, &cancel, true)
            .expect("unchanged discarded candidate must revalidate");

        let original_mtime = fs::metadata(&candidate)
            .expect("candidate metadata")
            .modified()
            .expect("candidate mtime");
        fs::write(&candidate, changed).expect("mutated candidate");
        File::options()
            .write(true)
            .open(&candidate)
            .expect("open candidate for timestamp restore")
            .set_times(FileTimes::new().set_modified(original_mtime))
            .expect("restore candidate mtime");

        let error = super::revalidate_path_record(&candidate, &record, &cancel, true)
            .expect_err("same-stamp discarded candidate mutation must stale");
        assert!(
            error.contains("changed") || error.contains("metadata"),
            "unexpected discarded candidate revalidation error: {error}"
        );
    }

    #[test]
    fn local_snapshot_rejects_a_cached_model_from_a_different_context() {
        let root = tempfile::tempdir().expect("workspace root");
        let uri =
            Url::from_file_path(root.path().join("ContextSnapshot.pas")).expect("fixture URI");
        let source = "unit ContextSnapshot;\ninterface\nimplementation\nend.\n";
        let mut workspace = test_workspace(vec![root.path().to_path_buf()], Default::default());
        workspace
            .open_document(uri.clone(), source.to_owned(), 1)
            .expect("open document");
        let mut input = workspace.analysis_input();
        let cached = input
            .cached_documents
            .get(&uri)
            .expect("indexed document should be available to snapshots")
            .parsed
            .clone();
        input
            .cached_documents
            .get_mut(&uri)
            .expect("cached context")
            .context
            .defines
            .push("FEATURE".to_owned());
        let cancel = AtomicBool::new(false);

        let snapshot = build_snapshot(
            &input,
            std::slice::from_ref(&uri),
            &[],
            SnapshotMode::Local,
            None,
            &[],
            &cancel,
        )
        .expect("local snapshot");
        let snapshot_document = snapshot
            .index
            .reusable_documents()
            .into_iter()
            .find(|(document_uri, _)| document_uri == &uri)
            .map(|(_, parsed)| parsed)
            .expect("snapshot document");

        assert!(
            !Arc::ptr_eq(&cached, &snapshot_document),
            "a cache from a different project context must not be reused"
        );
    }

    #[test]
    fn analysis_cache_follows_the_bounded_retained_file_set() {
        let root = tempfile::tempdir().expect("workspace root");
        let first_path = root.path().join("First.pas");
        let second_path = root.path().join("Second.pas");
        let first_uri = Url::from_file_path(&first_path).expect("first URI");
        let second_uri = Url::from_file_path(&second_path).expect("second URI");
        let first_source = "unit First;\ninterface\nimplementation\nend.\n";
        let second_source = "unit Second;\ninterface\nimplementation\nend.\n";
        fs::write(&first_path, first_source).expect("first source");
        fs::write(&second_path, second_source).expect("second source");
        let mut options = WorkspaceOptions::default();
        options.limits.max_files = 1;
        let mut workspace = test_workspace(vec![root.path().to_path_buf()], options);

        workspace
            .open_document(first_uri.clone(), first_source.to_owned(), 1)
            .expect("open first document");
        assert!(workspace.close_document(&first_uri));
        assert!(
            workspace
                .analysis_input()
                .cached_documents
                .contains_key(&first_uri),
            "the first document should be cached before eviction"
        );

        workspace
            .open_document(second_uri.clone(), second_source.to_owned(), 1)
            .expect("open second document");
        let input = workspace.analysis_input();
        assert!(input.cached_documents.contains_key(&second_uri));
        assert!(
            !input.cached_documents.contains_key(&first_uri),
            "evicted documents must not remain in the reusable cache"
        );
    }

    #[test]
    fn delphi_overrides_analysis_input_reuses_captured_layers() {
        let root = tempfile::tempdir().unwrap();
        let workspace = Workspace::with_override_session(
            vec![root.path().to_path_buf()],
            WorkspaceOptions::default(),
            OverrideSession::new(None),
        );
        let input = workspace.analysis_input();
        std::fs::write(
            root.path().join(".delphi-tools.local.toml"),
            "[properties]\nBDS = 'new-on-disk'\n",
        )
        .unwrap();
        let rebuilt = Workspace::with_override_session(
            input.roots.clone(),
            input.options.clone(),
            input.overrides.clone(),
        );
        assert!(
            rebuilt
                .overrides
                .effective_for(Some(root.path()), None)
                .unwrap()
                .properties
                .is_empty()
        );
    }

    #[test]
    fn enumeration_owner_indices_follow_path_sorting() {
        let mut enumeration = Enumeration {
            complete: true,
            ..Enumeration::default()
        };
        let b_path = PathBuf::from("/snapshot/B.pas");
        let a_path = PathBuf::from("/snapshot/A.pas");
        enumeration.add_path(b_path.clone(), None);
        enumeration.add_path(a_path.clone(), None);
        enumeration.sort_paths(&[]);
        let a_owner = ContextKey {
            project_file: Some(PathBuf::from("/projects/A.dproj")),
            workspace_root: None,
            project_scope: None,
            selection_scope: None,
            selection_project: None,
            config: None,
            platform: None,
            conditional_context: Default::default(),
            overrides: EffectiveOverrides::default(),
        };
        let b_owner = ContextKey {
            project_file: Some(PathBuf::from("/projects/B.dproj")),
            ..a_owner.clone()
        };
        enumeration.assign_owner(&a_path, a_owner.clone());
        enumeration.assign_owner(&b_path, b_owner.clone());

        assert_eq!(enumeration.paths[0].path, a_path);
        assert_eq!(enumeration.paths[0].owner, Some(a_owner));
        assert_eq!(enumeration.paths[1].path, b_path);
        assert_eq!(enumeration.paths[1].owner, Some(b_owner));
    }

    #[test]
    fn enumeration_rejects_changed_contexts_with_the_same_key() {
        let key = ContextKey {
            project_file: Some(PathBuf::from("/workspace/App.dproj")),
            workspace_root: Some(PathBuf::from("/workspace")),
            project_scope: Some(PathBuf::from("/workspace")),
            selection_scope: None,
            selection_project: None,
            config: None,
            platform: None,
            conditional_context: Default::default(),
            overrides: EffectiveOverrides::default(),
        };
        let old = ContextState {
            context: ProjectContext {
                search_paths: vec![PathBuf::from("/sdk/old")],
                ..ProjectContext::default()
            },
            ..ContextState::default()
        };
        let new = ContextState {
            context: ProjectContext {
                search_paths: vec![PathBuf::from("/sdk/new")],
                ..ProjectContext::default()
            },
            ..ContextState::default()
        };
        let mut enumeration = Enumeration {
            complete: true,
            ..Enumeration::default()
        };

        enumeration.retain_context(key.clone(), old);
        enumeration.retain_context(key, new);

        assert!(!enumeration.complete);
        assert!(
            enumeration
                .reason
                .as_deref()
                .is_some_and(|reason| reason.contains("changed"))
        );
    }

    #[test]
    fn snapshot_rejects_project_metadata_changed_between_priority_and_enumeration() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("workspace");
        let sdk_a = temp.path().join("sdk-a");
        let sdk_b = temp.path().join("sdk-b");
        let provider = root.join("Provider.pas");
        let project = root.join("App.dproj");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&sdk_a).unwrap();
        fs::create_dir_all(&sdk_b).unwrap();
        fs::write(
            &provider,
            "unit Provider; interface const SharedValue = 1; implementation end.\n",
        )
        .unwrap();
        fs::write(
            sdk_a.join("Consumer.pas"),
            "unit Consumer; interface uses Provider; implementation procedure Use; begin Log(SharedValue); end; end.\n",
        )
        .unwrap();
        fs::write(
            sdk_b.join("Consumer.pas"),
            "unit Consumer; interface uses Provider; implementation procedure Use; begin Log(SharedValue); end; end.\n",
        )
        .unwrap();
        let mapping = format!(
            "[[path_mappings]]\nfrom = 'C:\\SDK_A'\nto = '{}'\n[[path_mappings]]\nfrom = 'C:\\SDK_B'\nto = '{}'\n",
            sdk_a.display(),
            sdk_b.display(),
        );
        fs::write(root.join(".delphi-tools.local.toml"), mapping).unwrap();
        let old_project = "<Project><PropertyGroup><MainSource>Provider.pas</MainSource><DCC_UnitSearchPath>C:\\SDK_A</DCC_UnitSearchPath></PropertyGroup></Project>";
        let new_project = "<Project><PropertyGroup><MainSource>Provider.pas</MainSource><DCC_UnitSearchPath>C:\\SDK_B</DCC_UnitSearchPath></PropertyGroup></Project>";
        fs::write(&project, old_project).unwrap();

        let workspace = Workspace::with_override_session(
            vec![root.clone()],
            WorkspaceOptions {
                project_file: Some(project.clone()),
                ..WorkspaceOptions::default()
            },
            OverrideSession::new(None),
        );
        let input = workspace.analysis_input();
        let provider_uri = Url::from_file_path(&provider).unwrap();
        let (ready_sender, ready_receiver) = mpsc::channel();
        let (release_sender, release_receiver) = mpsc::channel();
        install_snapshot_priority_barrier(provider_uri.clone(), ready_sender, release_receiver);
        let project_for_mutator = project.clone();
        let mutator = thread::spawn(move || {
            ready_receiver.recv().expect("priority context capture");
            fs::write(project_for_mutator, new_project).expect("switch project search root");
            release_sender.send(()).expect("release snapshot barrier");
        });

        let cancel = AtomicBool::new(false);
        let snapshot = build_snapshot(
            &input,
            std::slice::from_ref(&provider_uri),
            &["SharedValue".to_string()],
            SnapshotMode::Workspace,
            None,
            &[],
            &cancel,
        )
        .expect("snapshot construction should report incompleteness, not panic");
        mutator.join().expect("metadata mutator must finish");

        assert!(!snapshot.complete);
        assert!(
            snapshot
                .incomplete_reason
                .as_deref()
                .is_some_and(|reason| reason.contains("context changed"))
        );
    }

    #[test]
    fn context_payload_baseline_uses_the_original_observation_after_equal_metadata_change() {
        let temp = tempfile::tempdir().expect("temporary workspace");
        let root = temp.path().to_path_buf();
        let metadata = root.join("App.dproj");
        let original =
            "<Project><PropertyGroup><MainSource>App.dpr</MainSource></PropertyGroup></Project>";
        let replacement =
            "<Project><PropertyGroup><MainSource>New.dpr</MainSource></PropertyGroup></Project>";
        assert_eq!(original.len(), replacement.len());
        fs::write(&metadata, original).expect("metadata");
        let original_metadata = fs::metadata(&metadata).expect("original metadata");
        let original_stamp = super::super::path_stamp(&metadata);
        let original_hash = super::super::content_hash_bytes(original.as_bytes());
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
        let later = MetadataObservation::Payload {
            path: metadata.clone(),
            read_policy: policy.clone(),
            path_entry: entry.clone(),
            stamp: None,
            content_hash: super::super::content_hash_bytes(replacement.as_bytes()),
        };
        let key = ContextKey {
            project_file: Some(metadata.clone()),
            workspace_root: Some(root.clone()),
            project_scope: Some(root.clone()),
            selection_scope: None,
            selection_project: None,
            config: None,
            platform: None,
            conditional_context: Default::default(),
            overrides: EffectiveOverrides::default(),
        };
        let workspace = test_workspace(vec![root.clone()], WorkspaceOptions::default());
        let mut workspace = workspace;
        workspace.contexts.insert(
            key.clone(),
            ContextState {
                context: ProjectContext {
                    metadata_files: vec![metadata.clone()],
                    metadata_observations: vec![
                        MetadataObservation::Payload {
                            path: metadata.clone(),
                            read_policy: policy.clone(),
                            path_entry: entry.clone(),
                            stamp: original_stamp.clone(),
                            content_hash: original_hash,
                        },
                        later,
                    ],
                    ..ProjectContext::default()
                },
                ..ContextState::default()
            },
        );

        fs::write(&metadata, replacement).expect("replacement metadata");
        File::options()
            .write(true)
            .open(&metadata)
            .expect("metadata for timestamp restore")
            .set_times(
                FileTimes::new().set_modified(
                    original_metadata
                        .modified()
                        .expect("original modification time"),
                ),
            )
            .expect("restore metadata timestamp");

        let mut baseline = BaselineAccumulator::default();
        let mut hashes = HashMap::new();
        let mut contents = HashMap::new();
        capture_context_baseline(
            &workspace,
            &key,
            &mut baseline,
            &mut hashes,
            &mut contents,
            false,
            &AtomicBool::new(false),
        )
        .expect("capture metadata baseline");

        let baseline_path = baseline
            .paths
            .iter()
            .find(|path| path.path == metadata)
            .expect("metadata baseline path");
        assert_eq!(baseline_path.stamp, original_stamp);
        assert_eq!(
            hashes.get(&super::path_key(&metadata)),
            Some(&original_hash)
        );
    }

    #[test]
    fn worker_rejects_a_recursive_membership_change_with_equal_file_metadata() {
        let temp = tempfile::tempdir().expect("temporary workspace");
        let root = temp.path().to_path_buf();
        let target = root.join("Child.pas");
        let original_source = "unit Child;\ninterface\nconst BadConst = 1;\nimplementation\nend.\n";
        let changed_source = original_source.replace("= 1", "= 2");
        fs::write(&target, original_source).expect("recursive member");
        fs::write(
            root.join("A.dpr"),
            "program A; uses Child in 'Child.pas'; begin end.\n",
        )
        .expect("owning main source");
        fs::write(root.join("B.dpr"), "program B; begin end.\n").expect("competing main source");
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
        let original_metadata = fs::metadata(&target).expect("target metadata");
        let target_uri = Url::from_file_path(&target).expect("target URI");
        let workspace = test_workspace(vec![root.clone()], WorkspaceOptions::default());
        let (ready_sender, ready_receiver) = mpsc::channel();
        let (release_sender, release_receiver) = mpsc::channel();
        install_snapshot_priority_barrier(target_uri.clone(), ready_sender, release_receiver);
        let target_for_mutator = target.clone();
        let mutator = thread::spawn(move || {
            ready_receiver.recv().expect("priority context capture");
            fs::write(&target_for_mutator, changed_source).expect("change recursive member");
            File::options()
                .write(true)
                .open(&target_for_mutator)
                .expect("target for timestamp restore")
                .set_times(
                    FileTimes::new().set_modified(
                        original_metadata
                            .modified()
                            .expect("original modification time"),
                    ),
                )
                .expect("restore target timestamp");
            release_sender.send(()).expect("release snapshot barrier");
        });

        let mut workspace = workspace;
        let result = workspace.rename_edits(&target_uri, Position::new(2, 6), "GOOD_CONST", false);
        mutator.join().expect("recursive member mutator");

        let error = result.expect_err(
            "a recursive membership payload changed after context evaluation; the worker must reject the snapshot",
        );
        assert!(
            error.contains("changed") || error.contains("metadata") || error.contains("stale"),
            "unexpected recursive metadata revalidation error: {error}"
        );
    }

    #[test]
    fn worker_revalidates_exists_only_stat_metadata_without_opening_its_payload() {
        let temp = tempfile::tempdir().expect("temporary workspace");
        let root = temp.path().to_path_buf();
        let private = root.join("vendor/private/settings.optset");
        let target = root.join("Provider.pas");
        let target_source =
            "unit Provider;\ninterface\nconst BadConst = 1;\nimplementation\nend.\n";
        fs::create_dir_all(private.parent().expect("private metadata parent"))
            .expect("private metadata directory");
        fs::write(&private, "<Project />").expect("Exists-only metadata");
        fs::write(&target, target_source).expect("target source");
        fs::write(root.join("App.dpr"), "program App; begin end.\n").expect("main source");
        fs::write(
            root.join("App.dproj"),
            "<Project><PropertyGroup><MainSource>App.dpr</MainSource></PropertyGroup><PropertyGroup Condition=\"Exists('vendor/private/settings.optset')\"><DCC_Define>PRIVATE_SETTINGS</DCC_Define></PropertyGroup></Project>",
        )
        .expect("project descriptor");

        let workspace = test_workspace(
            vec![root.clone()],
            WorkspaceOptions {
                source_paths: vec!["vendor".to_string()],
                exclude: vec!["vendor/private".to_string()],
                ..WorkspaceOptions::default()
            },
        );
        let input = workspace.analysis_input();
        let target_uri = Url::from_file_path(&target).expect("target URI");
        let cancel = AtomicBool::new(false);
        let computed = rename_from_input(
            input.clone(),
            &target_uri,
            Position::new(2, 6),
            "GOOD_CONST",
            false,
            &cancel,
        );
        assert!(
            computed.value.is_ok(),
            "complete worker baseline: {:?}",
            computed.value
        );
        assert!(
            computed.records.iter().any(|record| {
                record.path.as_deref() == Some(private.as_path())
                    && record.candidate_membership.is_none()
                    && record.content_hash.is_none()
                    && record.read_policy.is_none()
                    && record.path_entry.is_none()
            }),
            "Exists-only dependency must be retained as a stat-only record"
        );

        fs::remove_file(&private).expect("change Exists-only metadata state");
        let error = revalidate_input(&workspace.analysis_input(), &computed.records, &cancel)
            .expect_err("a changed Exists-only stat dependency must invalidate the snapshot");
        assert!(
            error.contains("changed") || error.contains("metadata"),
            "unexpected stat-only revalidation error: {error}"
        );
    }

    #[test]
    fn workspace_queries_reject_an_incomplete_non_priority_consumer_context() {
        let temp = tempfile::tempdir().expect("temporary workspace");
        let root = temp.path().join("workspace");
        let provider_root = root.join("provider");
        let consumer_root = root.join("consumer");
        let provider = provider_root.join("Provider.pas");
        let consumer = consumer_root.join("Consumer.pas");
        let provider_source =
            "unit Provider;\ninterface\nconst SharedValue = 1;\nimplementation\nend.\n";
        let consumer_source = "unit Consumer;\ninterface\nuses Provider;\nimplementation\nprocedure Use;\nbegin\n  Log(SharedValue);\nend;\nend.\n";
        fs::create_dir_all(&provider_root).expect("provider directory");
        fs::create_dir_all(&consumer_root).expect("consumer directory");
        fs::write(&provider, provider_source).expect("provider source");
        fs::write(&consumer, consumer_source).expect("consumer source");
        fs::write(
            provider_root.join("Provider.dproj"),
            "<Project><PropertyGroup><MainSource>Provider.pas</MainSource></PropertyGroup></Project>",
        )
        .expect("provider project");
        for project in ["A.dproj", "B.dproj"] {
            fs::write(
                consumer_root.join(project),
                "<Project><PropertyGroup><MainSource>Consumer.pas</MainSource></PropertyGroup></Project>",
            )
            .expect("ambiguous consumer project");
        }

        let provider_uri = Url::from_file_path(&provider).expect("provider URI");
        let workspace = test_workspace(vec![root], WorkspaceOptions::default());
        let input = workspace.analysis_input();
        let cancel = AtomicBool::new(false);
        let provider_owner = super::owner_for_input(&input, &provider_uri, &cancel)
            .expect("priority provider context");
        assert!(
            provider_owner.state.context.discovery_complete,
            "priority provider context must be complete"
        );
        let references = super::super::queries::references_from_input(
            input.clone(),
            &provider_uri,
            Position::new(2, 6),
            true,
            &cancel,
        );
        assert!(
            references.value.is_err(),
            "references must fail rather than bind an ambiguous consumer: {references:?}"
        );

        let rename = rename_from_input(
            input,
            &provider_uri,
            Position::new(2, 6),
            "RenamedValue",
            false,
            &cancel,
        );
        assert!(
            rename.value.is_err(),
            "rename must fail rather than edit through an ambiguous consumer: {rename:?}"
        );
    }

    #[test]
    fn workspace_queries_reject_a_missing_optset_in_a_non_priority_consumer_context() {
        let temp = tempfile::tempdir().expect("temporary workspace");
        let root = temp.path().join("workspace");
        let provider_root = root.join("provider");
        let consumer_root = root.join("consumer");
        let provider = provider_root.join("Provider.pas");
        let consumer = consumer_root.join("Consumer.pas");
        let provider_source =
            "unit Provider;\ninterface\nconst SharedValue = 1;\nimplementation\nend.\n";
        let consumer_source = "unit Consumer;\ninterface\nuses Provider;\nimplementation\nprocedure Use;\nbegin\n  Log(SharedValue);\nend;\nend.\n";
        fs::create_dir_all(&provider_root).expect("provider directory");
        fs::create_dir_all(&consumer_root).expect("consumer directory");
        fs::write(&provider, provider_source).expect("provider source");
        fs::write(&consumer, consumer_source).expect("consumer source");
        fs::write(
            provider_root.join("Provider.dproj"),
            "<Project><PropertyGroup><MainSource>Provider.pas</MainSource></PropertyGroup></Project>",
        )
        .expect("provider project");
        fs::write(
            consumer_root.join("Consumer.dproj"),
            "<Project><PropertyGroup><MainSource>Consumer.pas</MainSource></PropertyGroup><ItemGroup><DCCReference Include=\"../provider/Provider.pas\" /></ItemGroup><Import Project=\"missing.optset\" /></Project>",
        )
        .expect("consumer project");

        let provider_uri = Url::from_file_path(&provider).expect("provider URI");
        let workspace = test_workspace(vec![root], WorkspaceOptions::default());
        let input = workspace.analysis_input();
        let cancel = AtomicBool::new(false);
        let provider_owner = super::owner_for_input(&input, &provider_uri, &cancel)
            .expect("priority provider context");
        assert!(
            provider_owner.state.context.discovery_complete,
            "priority provider context must be complete"
        );
        let references = super::super::queries::references_from_input(
            input.clone(),
            &provider_uri,
            Position::new(2, 6),
            true,
            &cancel,
        );
        assert!(
            references.value.is_err(),
            "references must fail rather than bind a missing-optset consumer: {references:?}"
        );

        let rename = rename_from_input(
            input,
            &provider_uri,
            Position::new(2, 6),
            "RenamedValue",
            false,
            &cancel,
        );
        assert!(
            rename.value.is_err(),
            "rename must fail rather than edit through a missing-optset consumer: {rename:?}"
        );
    }

    #[test]
    fn workspace_snapshot_rejects_an_incomplete_external_overlay_context() {
        let temp = tempfile::tempdir().expect("temporary workspace");
        let root = temp.path().join("workspace");
        let external = temp.path().join("external");
        let project = root.join("Provider.dproj");
        let overlay_path = external.join("Consumer.pas");
        fs::create_dir_all(&root).expect("workspace directory");
        fs::create_dir_all(&external).expect("external directory");

        let key = ContextKey {
            project_file: Some(project),
            workspace_root: Some(root.clone()),
            project_scope: Some(root.clone()),
            selection_scope: None,
            selection_project: None,
            config: None,
            platform: None,
            conditional_context: Default::default(),
            overrides: EffectiveOverrides::default(),
        };
        let state = ContextState {
            context: ProjectContext {
                discovery_complete: false,
                search_paths: vec![external.clone()],
                search_path_entries: vec![super::super::super::project::ProjectPathEntry {
                    path: external,
                    provenance: super::super::super::project::ProjectPathProvenance::LegacyNative,
                }],
                ..ProjectContext::default()
            },
            ..ContextState::default()
        };
        let mut workspace = test_workspace(vec![root], WorkspaceOptions::default());
        workspace.contexts.insert(key.clone(), state.clone());
        let overlay_uri = Url::from_file_path(&overlay_path).expect("overlay URI");
        let mut input = workspace.analysis_input();
        input.overlays.insert(
            overlay_uri.clone(),
            super::OverlayInput {
                text: "unit Consumer; interface end.".to_string(),
                version: 1,
            },
        );
        input.document_owners.insert(
            overlay_uri,
            super::super::KnownDocumentOwner {
                key: key.clone(),
                state,
                origin: super::super::OwnerOrigin::Inherited,
                needs_revalidation: false,
                follow_current_project_file: false,
                legacy_route: None,
            },
        );
        let mut enumeration = Enumeration {
            complete: true,
            ..Enumeration::default()
        };

        enumerate_external_overlays(
            &mut workspace,
            &input,
            &HashSet::from([key]),
            SnapshotMode::Workspace,
            &mut enumeration,
            &AtomicBool::new(false),
        )
        .expect("external overlay enumeration");

        assert!(
            !enumeration.complete,
            "an incomplete external overlay context must block a workspace snapshot"
        );
    }

    #[test]
    fn enumeration_owner_assignment_uses_keyed_lookup_after_sort() {
        const SOURCES: usize = 2_048;
        let mut enumeration = Enumeration {
            complete: true,
            ..Enumeration::default()
        };
        let owner = ContextKey {
            project_file: Some(PathBuf::from("/projects/Owner.dproj")),
            workspace_root: None,
            project_scope: None,
            selection_scope: None,
            selection_project: None,
            config: None,
            platform: None,
            conditional_context: Default::default(),
            overrides: EffectiveOverrides::default(),
        };
        for index in (0..SOURCES).rev() {
            enumeration.add_path(PathBuf::from(format!("/snapshot/{index:04}.pas")), None);
        }
        enumeration.sort_paths(&[]);
        for index in 0..SOURCES {
            enumeration.assign_owner(
                &PathBuf::from(format!("/snapshot/{index:04}.pas")),
                owner.clone(),
            );
        }

        assert_eq!(enumeration.paths.len(), SOURCES);
        assert!(
            enumeration
                .paths
                .iter()
                .all(|source| { source.owner.as_ref() == Some(&owner) })
        );
        assert!(
            enumeration.lookup_count() <= SOURCES * 2,
            "enumeration lookup work must remain linear"
        );
    }

    #[test]
    fn contains_any_identifier_uses_identifier_boundaries() {
        let names = ["Foo".to_string(), "&Bar".to_string()];

        assert!(contains_any_identifier("value := FOO; &bar := 1;", &names));
        assert!(!contains_any_identifier("value := Foobar;", &names));
    }

    #[test]
    fn baseline_accumulator_merges_observations_with_linear_lookup_work() {
        const OBSERVATIONS: usize = 512;
        let membership = ProjectCandidateMembership {
            paths: vec![PathBuf::from("Project.dproj")],
            readable: true,
        };
        let stamp = PathStamp {
            bytes: 1,
            modified: None,
            is_dir: true,
            is_symlink: false,
        };

        let mut candidate_first = BaselineAccumulator::default();
        for index in 0..OBSERVATIONS {
            let path = PathBuf::from(format!("/snapshot/candidate-first-{index}"));
            candidate_first.add_candidate_membership(path.clone(), membership.clone(), false);
            candidate_first.add_path(path, Some(stamp.clone()));
        }

        let mut stamp_first = BaselineAccumulator::default();
        for index in 0..OBSERVATIONS {
            let path = PathBuf::from(format!("/snapshot/stamp-first-{index}"));
            stamp_first.add_path(path.clone(), Some(stamp.clone()));
            stamp_first.add_candidate_membership(path, membership.clone(), false);
        }

        for accumulator in [&candidate_first, &stamp_first] {
            assert_eq!(accumulator.paths.len(), OBSERVATIONS);
            assert_eq!(
                accumulator.lookup_count(),
                OBSERVATIONS * 2,
                "each observation should perform one keyed lookup, not a baseline scan"
            );
            assert!(accumulator.paths.iter().all(|baseline| {
                baseline.stamp == Some(stamp.clone())
                    && baseline.candidate_membership == Some(membership.clone())
            }));
        }
    }

    #[test]
    fn candidate_membership_outside_workspace_does_not_capture_a_directory_stamp() {
        let temp = tempfile::tempdir().expect("temporary directory");
        let workspace_root = temp.path().join("workspace");
        let directory = temp.path().join("external");
        fs::create_dir(&workspace_root).expect("workspace root");
        fs::create_dir(&directory).expect("candidate directory");
        let key = ContextKey {
            project_file: None,
            workspace_root: None,
            project_scope: None,
            selection_scope: None,
            selection_project: None,
            config: None,
            platform: None,
            conditional_context: Default::default(),
            overrides: EffectiveOverrides::default(),
        };
        let membership = ProjectCandidateMembership {
            paths: Vec::new(),
            readable: true,
        };
        let mut workspace = test_workspace(vec![workspace_root], WorkspaceOptions::default());
        workspace.contexts.insert(
            key.clone(),
            ContextState {
                project_candidate_memberships: HashMap::from([(
                    directory.clone(),
                    Ok(membership.clone()),
                )]),
                ..ContextState::default()
            },
        );
        let mut baseline = BaselineAccumulator::default();
        let mut hashes = HashMap::new();
        let mut contents = HashMap::new();

        capture_context_baseline(
            &workspace,
            &key,
            &mut baseline,
            &mut hashes,
            &mut contents,
            true,
            &AtomicBool::new(false),
        )
        .expect("candidate membership baseline");

        let baseline_path = baseline
            .paths
            .iter()
            .find(|path| path.path == directory)
            .expect("candidate directory baseline");
        assert_eq!(baseline_path.candidate_membership, Some(membership));
        assert_eq!(baseline_path.stamp, None);
    }

    #[test]
    fn structural_symbol_snapshot_does_not_audit_include_dependencies() {
        let temp = tempfile::tempdir().expect("temporary workspace");
        let root = temp.path().join("fixture");
        let source_path = root.join("Main.pas");
        let missing_include = root.join("Missing.inc");
        fs::create_dir_all(&root).expect("fixture directory");
        fs::write(
            &source_path,
            "unit Main;\ninterface\nprocedure VisibleThing;\nimplementation\n{$I Missing.inc}\nend.\n",
        )
        .expect("source");

        let workspace = test_workspace(vec![root], WorkspaceOptions::default());
        let input = workspace.analysis_input();
        let cancel = AtomicBool::new(false);
        let snapshot = build_snapshot(
            &input,
            &[],
            &[],
            SnapshotMode::WorkspaceSymbols,
            None,
            &[],
            &cancel,
        )
        .expect("structural snapshot");
        assert!(
            snapshot
                .records
                .keys()
                .any(|uri| uri.to_file_path().ok().as_deref() == Some(source_path.as_path()))
        );
        assert!(
            !snapshot
                .baseline_records
                .iter()
                .any(|record| { record.path.as_deref() == Some(missing_include.as_path()) })
        );
    }

    #[test]
    fn consumed_configuration_baseline_retains_the_classification_observation() {
        let temp = tempfile::tempdir().expect("temporary workspace");
        let path = temp.path().join("App.dproj");
        fs::write(&path, b"actual").expect("configuration");
        let observed_bytes = b"observed".to_vec();
        let observed_stamp = PathStamp {
            bytes: observed_bytes.len() as u64,
            modified: None,
            is_dir: false,
            is_symlink: false,
        };
        let record = path_record_at(
            path.clone(),
            Some(observed_stamp.clone()),
            None,
            Some(observed_bytes.clone()),
            None,
            None,
            None,
            false,
        )
        .expect("configuration record");
        let mut baseline = BaselineAccumulator::default();
        let mut baseline_content_hashes = HashMap::new();
        let mut baseline_contents = HashMap::new();
        let cancel = AtomicBool::new(false);

        capture_consumed_configuration_baseline(
            &[record],
            &mut baseline,
            &mut baseline_content_hashes,
            &mut baseline_contents,
            &cancel,
        )
        .expect("classification observation baseline");

        assert_eq!(
            baseline_contents.get(&path_key(&path)),
            Some(&observed_bytes)
        );
        assert_eq!(baseline.paths[0].stamp, Some(observed_stamp));
    }

    #[test]
    fn include_analysis_honors_cancellation() {
        let temp = tempfile::tempdir().expect("temporary workspace");
        let root = temp.path().join("fixture");
        let source_path = root.join("Main.pas");
        fs::create_dir_all(&root).expect("fixture directory");
        fs::write(
            &source_path,
            "unit Main;\ninterface\nprocedure VisibleThing;\nimplementation\n{$I Nested.inc}\nend.\n",
        )
        .expect("source");
        fs::write(
            root.join("Nested.inc"),
            "{$IFDEF MAYBE}\nHidden\n{$ENDIF}\n",
        )
        .expect("include");

        let workspace = Workspace::new(vec![root], WorkspaceOptions::default());
        let input = workspace.analysis_input();
        let uri = Url::from_file_path(&source_path).expect("source URI");
        let cancel = AtomicBool::new(false);
        let _guard = test_cancel_in_include_analysis();
        let result = build_snapshot(
            &input,
            std::slice::from_ref(&uri),
            &[],
            SnapshotMode::LocalWithImports,
            None,
            &[],
            &cancel,
        );
        assert_eq!(result.err().as_deref(), Some("request cancelled"));
    }

    #[test]
    fn include_payload_content_hashes_reject_changed_snapshot_inputs() {
        let temp = tempfile::tempdir().expect("temporary workspace");
        let root = temp.path().join("fixture");
        let provider = root.join("Provider.pas");
        let consumer = root.join("Consumer.pas");
        let include = root.join("Body.inc");
        let provider_source =
            "unit Provider;\ninterface\nconst badConst = 1;\nimplementation\nend.\n";
        let consumer_source = "unit Consumer;\ninterface\nuses Provider;\nimplementation\n{$I Body.inc}\nprocedure Use;\nbegin\n  Log(badConst);\nend;\nend.\n";
        fs::create_dir_all(&root).expect("fixture directory");
        fs::write(&provider, provider_source).expect("provider source");
        fs::write(&consumer, consumer_source).expect("consumer source");
        fs::write(&include, "{$DEFINE SAFE}\n").expect("include source");

        let workspace = test_workspace(vec![root.clone()], WorkspaceOptions::default());
        let input = workspace.analysis_input();
        let provider_uri = Url::from_file_path(&provider).expect("provider URI");
        let cancel = AtomicBool::new(false);
        let snapshot = build_snapshot(
            &input,
            std::slice::from_ref(&provider_uri),
            &["badConst".to_owned(), "GoodConst".to_owned()],
            SnapshotMode::Workspace,
            None,
            &[],
            &cancel,
        )
        .expect("include snapshot");
        let records = snapshot_records(&snapshot);
        assert!(records.iter().any(|record| {
            record.path.as_deref().is_some_and(|path| path == include) && record.include_payload
        }));

        fs::write(&include, "{$DEFINE CHANGED}\n").expect("changed include source");
        let error = revalidate_input(&input, &records, &cancel)
            .expect_err("changed include payload must stale the snapshot");
        assert!(
            error.contains("Body.inc") || error.contains("changed"),
            "unexpected include revalidation error: {error}"
        );
    }

    #[test]
    fn structural_symbol_snapshot_reports_a_source_byte_limit_exhaustion() {
        let temp = tempfile::tempdir().expect("temporary workspace");
        let root = temp.path().join("fixture");
        let source_path = root.join("Main.pas");
        let source = "unit Main;\ninterface\nprocedure VisibleThing;\nimplementation\nend.\n";
        fs::create_dir_all(&root).expect("fixture directory");
        fs::write(&source_path, source).expect("source");

        let mut options = WorkspaceOptions::default();
        options.limits.max_file_bytes = source.len() - 1;
        let workspace = test_workspace(vec![root], options);
        let input = workspace.analysis_input();
        let cancel = AtomicBool::new(false);
        let snapshot = build_snapshot(
            &input,
            &[],
            &[],
            SnapshotMode::WorkspaceSymbols,
            None,
            &[],
            &cancel,
        )
        .expect("a bounded structural snapshot returns its completeness state");
        assert!(!snapshot.complete);
        assert!(
            snapshot
                .index
                .workspace_symbols("")
                .expect("empty structural index query")
                .is_empty()
        );
        assert!(
            snapshot
                .incomplete_reason
                .as_deref()
                .is_some_and(|reason| reason.contains("per-file limit"))
        );
    }

    #[test]
    fn structural_symbol_snapshot_revalidates_content_and_membership() {
        let temp = tempfile::tempdir().expect("temporary workspace");
        let root = temp.path().join("fixture");
        let source_path = root.join("Main.pas");
        let source = "unit Main;\ninterface\nprocedure VisibleThing;\nimplementation\nend.\n";
        fs::create_dir_all(&root).expect("fixture directory");
        fs::write(&source_path, source).expect("source");

        let workspace = test_workspace(vec![root.clone()], WorkspaceOptions::default());
        let input = workspace.analysis_input();
        let cancel = AtomicBool::new(false);
        let snapshot = build_snapshot(
            &input,
            &[],
            &[],
            SnapshotMode::WorkspaceSymbols,
            None,
            &[],
            &cancel,
        )
        .expect("structural snapshot");
        let records = snapshot_records(&snapshot);

        fs::write(
            &source_path,
            "unit Main;\ninterface\nprocedure ChangedThing;\nimplementation\nend.\n",
        )
        .expect("changed source");
        let error = revalidate_input(&input, &records, &cancel)
            .expect_err("changed source content must invalidate symbol results");
        assert!(error.contains("changed"));

        fs::write(&source_path, source).expect("restore source");
        let snapshot = build_snapshot(
            &input,
            &[],
            &[],
            SnapshotMode::WorkspaceSymbols,
            None,
            &[],
            &cancel,
        )
        .expect("restored structural snapshot");
        let records = snapshot_records(&snapshot);
        fs::remove_file(&source_path).expect("remove source");
        let error = revalidate_input(&input, &records, &cancel)
            .expect_err("removed source membership must invalidate symbol results");
        assert!(error.contains("changed") || error.contains("membership"));
    }

    #[test]
    fn structural_symbol_snapshot_rejects_new_source_membership() {
        let temp = tempfile::tempdir().expect("temporary workspace");
        let root = temp.path().join("fixture");
        let source_path = root.join("Main.pas");
        let added_path = root.join("Added.pas");
        let source = "unit Main;\ninterface\nprocedure VisibleThing;\nimplementation\nend.\n";
        fs::create_dir_all(&root).expect("fixture directory");
        fs::write(&source_path, source).expect("source");

        let workspace = test_workspace(vec![root.clone()], WorkspaceOptions::default());
        let input = workspace.analysis_input();
        let cancel = AtomicBool::new(false);
        let snapshot = build_snapshot(
            &input,
            &[],
            &[],
            SnapshotMode::WorkspaceSymbols,
            None,
            &[],
            &cancel,
        )
        .expect("structural snapshot");
        let records = snapshot_records(&snapshot);
        assert!(
            records
                .iter()
                .any(|record| record.path.as_deref() == Some(root.as_path())),
            "the source directory must be part of the structural read set"
        );
        assert!(
            !records.iter().any(|record| {
                record.uri.to_file_path().ok().as_deref() == Some(added_path.as_path())
            }),
            "the added source must be unobserved when the snapshot is built"
        );
        assert!(
            revalidate_input(&input, &records, &cancel).is_ok(),
            "an unchanged structural snapshot must remain valid"
        );

        fs::write(
            &added_path,
            "unit Added;\ninterface\nprocedure VisibleThing;\nimplementation\nend.\n",
        )
        .expect("matching added source");
        let error = revalidate_input(&input, &records, &cancel)
            .expect_err("new source membership must invalidate symbol results");
        assert!(
            error.contains("membership") || error.contains("metadata"),
            "directory membership change must be reported explicitly: {error}"
        );
    }

    #[test]
    fn rename_revalidation_observes_a_new_pascal_consumer() {
        let temp = tempfile::tempdir().expect("temporary workspace");
        let root = temp.path().join("fixture");
        let provider = root.join("Provider.pas");
        let provider_source =
            "unit Provider;\ninterface\nconst badConst = 1;\nimplementation\nend.\n";
        fs::create_dir_all(&root).expect("fixture directory");
        fs::write(&provider, provider_source).expect("provider source");

        let provider_uri = Url::from_file_path(&provider).expect("provider URI");
        let workspace = test_workspace(vec![root.clone()], WorkspaceOptions::default());
        let input = workspace.analysis_input();
        let cancel = AtomicBool::new(false);
        let computed = rename_from_input(
            input.clone(),
            &provider_uri,
            Position::new(2, 6),
            "BAD_CONST",
            false,
            &cancel,
        );
        assert!(
            computed.value.is_ok(),
            "rename must compute before the consumer is added: {:?}",
            computed.value
        );
        revalidate_input(&input, &computed.records, &cancel)
            .expect("unchanged rename inputs must remain valid");

        let added_consumer = root.join("AddedConsumer.pas");
        fs::write(
            &added_consumer,
            "unit AddedConsumer;\ninterface\nuses Provider;\nimplementation\nprocedure Run;\nbegin\n  Log(badConst);\nend;\nend.\n",
        )
        .expect("new Pascal consumer");

        let error = revalidate_input(&input, &computed.records, &cancel)
            .expect_err("a new Pascal consumer must stale the rename plan");
        assert!(
            error.contains("membership") || error.contains("changed"),
            "unexpected new-consumer rename revalidation error: {error}"
        );
    }

    #[test]
    fn rename_revalidation_observes_package_metadata_discovered_during_binding() {
        let temp = tempfile::tempdir().expect("temporary workspace");
        let root = temp.path().join("fixture");
        let package_root = temp.path().join("packages");
        let imported_metadata = package_root.join("metadata/Mappings.optset");
        let provider = root.join("Provider.pas");
        let consumer = root.join("Main.pas");
        let project = root.join("App.dproj");
        let package_project = package_root.join("Package.dproj");
        let package_source = package_root.join("PackageMain.dpk");
        let provider_source =
            "unit Provider;\ninterface\nconst BadConst = 1;\nimplementation\nend.\n";
        let consumer_source = "unit Main;\ninterface\nuses Provider, PackagedUnit;\nimplementation\nprocedure Run;\nbegin\n  Log(BadConst);\nend;\nend.\n";
        fs::create_dir_all(&root).expect("project directory");
        fs::create_dir_all(&package_root).expect("package directory");
        fs::create_dir_all(imported_metadata.parent().expect("metadata parent"))
            .expect("metadata directory");
        fs::write(&provider, provider_source).expect("package unit");
        fs::write(&consumer, consumer_source).expect("consumer source");
        fs::write(
            &project,
            "<Project><PropertyGroup><MainSource>Main.pas</MainSource><DCC_UsePackage>Package</DCC_UsePackage></PropertyGroup></Project>",
        )
        .expect("project");
        fs::write(&package_source, "package PackageMain;\ncontains\nend.\n")
            .expect("package source");
        fs::write(
            package_root.join("PackagedUnit.pas"),
            "unit PackagedUnit;\ninterface\nimplementation\nend.\n",
        )
        .expect("package unit");
        fs::write(
            &package_project,
             "<Project><PropertyGroup><MainSource>PackageMain.dpk</MainSource></PropertyGroup><Import Project=\"metadata/Mappings.optset\" /></Project>",
        )
        .expect("package project");
        fs::write(
            &imported_metadata,
            format!(
                "<Project><ItemGroup><DCCReference Include=\"{}\" /></ItemGroup></Project>",
                package_root.join("PackagedUnit.pas").display()
            ),
        )
        .expect("imported package metadata");
        fs::write(
            root.join(".delphi-tools.local.toml"),
            format!(
                "[[path_mappings]]\nfrom = 'C:\\Packages'\nto = '{}'\n",
                package_root.display()
            ),
        )
        .expect("mapping configuration");

        let provider_uri = Url::from_file_path(&provider).expect("provider URI");
        let workspace = test_workspace(vec![root], WorkspaceOptions::default());
        let input = workspace.analysis_input();
        let cancel = AtomicBool::new(false);
        let computed = rename_from_input(
            input,
            &provider_uri,
            Position::new(2, 6),
            "GoodConst",
            false,
            &cancel,
        );
        assert!(
            computed.value.is_ok(),
            "rename must compute: {:?}",
            computed.value
        );
        assert!(
            computed
                .records
                .iter()
                .any(|record| record.path.as_deref() == Some(imported_metadata.as_path())),
            "package metadata discovered during import binding must be retained in the read-set"
        );
        fs::write(
            &imported_metadata,
            "<Project><ItemGroup><DCCReference Include=\"missing.pas\" /></ItemGroup></Project>",
        )
        .expect("changed imported package metadata");
        let error = revalidate_input(&workspace.analysis_input(), &computed.records, &cancel)
            .expect_err("a package descriptor changed during binding must stale the rename");
        assert!(
            error.contains("changed") || error.contains("metadata"),
            "unexpected package metadata revalidation error: {error}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn in_flight_references_reject_a_changed_mapped_overlay() {
        let temp = tempfile::tempdir().expect("temporary workspace");
        let root = temp.path().join("project");
        let sdk = temp.path().join("sdk");
        let project = root.join("App.dproj");
        let provider = root.join("Provider.pas");
        let consumer = sdk.join("Consumer.pas");
        let provider_source =
            "unit Provider;\ninterface\nconst BadConst = 1;\nimplementation\nend.\n";
        let consumer_source = "unit Consumer;\ninterface\nuses Provider;\nimplementation\nprocedure Consume;\nbegin\n  Log(BadConst);\nend;\nend.\n";
        fs::create_dir_all(&root).expect("project directory");
        fs::create_dir_all(&sdk).expect("mapped source directory");
        fs::write(&provider, provider_source).expect("provider source");
        fs::write(
            &project,
            "<Project><PropertyGroup><MainSource>Provider.pas</MainSource><DCC_UnitSearchPath>C:\\SDK</DCC_UnitSearchPath></PropertyGroup></Project>",
        )
        .expect("project");
        fs::write(
            root.join(".delphi-tools.local.toml"),
            format!(
                "[[path_mappings]]\nfrom = 'C:\\SDK'\nto = '{}'\n",
                sdk.display()
            ),
        )
        .expect("mapping configuration");

        let provider_uri = Url::from_file_path(&provider).expect("provider URI");
        let consumer_uri = Url::from_file_path(&consumer).expect("consumer URI");
        let mut workspace = test_workspace(
            vec![root],
            WorkspaceOptions {
                project_file: Some(project),
                ..WorkspaceOptions::default()
            },
        );
        workspace
            .open_document(consumer_uri.clone(), consumer_source.to_owned(), 1)
            .expect("open mapped consumer overlay");
        let input = workspace.analysis_input();
        let cancel = AtomicBool::new(false);
        let computed = super::super::queries::references_from_input(
            input,
            &provider_uri,
            Position::new(2, 6),
            false,
            &cancel,
        );
        assert!(
            computed.value.is_ok(),
            "rename must compute: {:?}",
            computed.value
        );
        assert!(
            computed
                .records
                .iter()
                .any(|record| record.uri == consumer_uri)
        );

        workspace
            .change_document(
                consumer_uri,
                "unit Consumer;\ninterface\nuses Provider;\nimplementation\nend.\n".to_owned(),
                2,
            )
            .expect("change mapped consumer overlay");
        let error = workspace
            .finish_computation(computed)
            .expect_err("changed mapped overlay must stale the computed rename");
        assert!(error.contains("stale") || error.contains("changed"));
    }

    #[test]
    fn in_flight_edit_rejects_a_changed_native_source() {
        let temp = tempfile::tempdir().expect("temporary workspace");
        let root = temp.path().join("project");
        let provider = root.join("Provider.pas");
        let consumer = root.join("Consumer.pas");
        let provider_source =
            "unit Provider;\ninterface\nconst BadConst = 1;\nimplementation\nend.\n";
        let consumer_source = "unit Consumer;\ninterface\nuses Provider;\nimplementation\nprocedure Consume;\nbegin\n  Log(BadConst);\nend;\nend.\n";
        fs::create_dir_all(&root).expect("project directory");
        fs::write(&provider, provider_source).expect("provider source");
        fs::write(&consumer, consumer_source).expect("consumer source");

        let provider_uri = Url::from_file_path(&provider).expect("provider URI");
        let workspace = test_workspace(vec![root], WorkspaceOptions::default());
        let input = workspace.analysis_input();
        let cancel = AtomicBool::new(false);
        let computed = rename_from_input(
            input,
            &provider_uri,
            Position::new(2, 6),
            "GoodConst",
            false,
            &cancel,
        );
        assert!(
            computed.value.is_ok(),
            "rename must compute: {:?}",
            computed.value
        );
        assert!(
            computed.records.iter().any(|record| {
                record.uri == Url::from_file_path(&consumer).expect("consumer URI")
            })
        );

        fs::write(
            &consumer,
            "unit Consumer;\ninterface\nuses Provider;\nimplementation\nend.\n",
        )
        .expect("changed native consumer");
        let error = workspace
            .finish_computation(computed)
            .expect_err("changed native source must stale the computed rename");
        assert!(error.contains("changed"));
    }

    #[test]
    fn in_flight_edit_rejects_changed_project_metadata() {
        let temp = tempfile::tempdir().expect("temporary workspace");
        let root = temp.path().join("project");
        let project = root.join("App.dproj");
        let provider = root.join("Provider.pas");
        let consumer = root.join("Consumer.pas");
        let provider_source =
            "unit Provider;\ninterface\nconst BadConst = 1;\nimplementation\nend.\n";
        let consumer_source = "unit Consumer;\ninterface\nuses Provider;\nimplementation\nprocedure Consume;\nbegin\n  Log(BadConst);\nend;\nend.\n";
        fs::create_dir_all(&root).expect("project directory");
        fs::write(&provider, provider_source).expect("provider source");
        fs::write(&consumer, consumer_source).expect("consumer source");
        fs::write(
            &project,
            "<Project><PropertyGroup><MainSource>Provider.pas</MainSource></PropertyGroup></Project>",
        )
        .expect("project");

        let provider_uri = Url::from_file_path(&provider).expect("provider URI");
        let workspace = test_workspace(vec![root], WorkspaceOptions::default());
        let input = workspace.analysis_input();
        let cancel = AtomicBool::new(false);
        let computed = rename_from_input(
            input,
            &provider_uri,
            Position::new(2, 6),
            "GoodConst",
            false,
            &cancel,
        );
        assert!(
            computed.value.is_ok(),
            "rename must compute: {:?}",
            computed.value
        );
        assert!(
            computed
                .records
                .iter()
                .any(|record| { record.path.as_deref() == Some(project.as_path()) })
        );

        fs::write(
            &project,
            "<Project><PropertyGroup><MainSource>Provider.pas</MainSource><DCC_Define>CHANGED</DCC_Define></PropertyGroup></Project>",
        )
        .expect("changed project metadata");
        let error = workspace
            .finish_computation(computed)
            .expect_err("changed project metadata must stale the computed rename");
        assert!(error.contains("changed") || error.contains("configuration"));
    }

    #[cfg(unix)]
    #[test]
    fn in_flight_edit_ignores_an_override_only_edit_with_captured_settings() {
        let temp = tempfile::tempdir().expect("temporary workspace");
        let root = temp.path().join("project");
        let sdk = temp.path().join("sdk");
        let changed_sdk = temp.path().join("changed-sdk");
        let project = root.join("App.dproj");
        let provider = root.join("Provider.pas");
        let consumer = root.join("Consumer.pas");
        let provider_source =
            "unit Provider;\ninterface\nconst BadConst = 1;\nimplementation\nend.\n";
        let consumer_source = "unit Consumer;\ninterface\nuses Provider;\nimplementation\nprocedure Consume;\nbegin\n  Log(BadConst);\nend;\nend.\n";
        fs::create_dir_all(&root).expect("project directory");
        fs::create_dir_all(&sdk).expect("mapped source directory");
        fs::create_dir_all(&changed_sdk).expect("replacement source directory");
        fs::write(&provider, provider_source).expect("provider source");
        fs::write(&consumer, consumer_source).expect("consumer source");
        fs::write(
            &project,
            "<Project><PropertyGroup><MainSource>Provider.pas</MainSource><DCC_UnitSearchPath>C:\\SDK</DCC_UnitSearchPath></PropertyGroup></Project>",
        )
        .expect("project");
        let overrides = root.join(".delphi-tools.local.toml");
        fs::write(
            &overrides,
            format!(
                "[[path_mappings]]\nfrom = 'C:\\SDK'\nto = '{}'\n",
                sdk.display()
            ),
        )
        .expect("mapping configuration");

        let provider_uri = Url::from_file_path(&provider).expect("provider URI");
        let mut workspace = test_workspace(
            vec![root],
            WorkspaceOptions {
                project_file: Some(project),
                ..WorkspaceOptions::default()
            },
        );
        let input = workspace.analysis_input();
        let cancel = AtomicBool::new(false);
        let computed = rename_from_input(
            input,
            &provider_uri,
            Position::new(2, 6),
            "GoodConst",
            false,
            &cancel,
        );
        assert!(
            computed.value.is_ok(),
            "rename must compute: {:?}",
            computed.value
        );

        fs::write(
            &overrides,
            format!(
                "[[path_mappings]]\nfrom = 'C:\\SDK'\nto = '{}'\n",
                changed_sdk.display()
            ),
        )
        .expect("changed override configuration");
        let overrides_uri = Url::from_file_path(&overrides).expect("override URI");
        workspace.file_event(&overrides_uri, FileChange::Changed);
        let result = workspace.finish_computation(computed);
        assert!(
            result.is_ok(),
            "captured override-only edits must not stale the computed rename: {result:?}"
        );
    }

    #[test]
    fn rename_revalidation_preserves_include_lookup_directory_stamps() {
        let temp = tempfile::tempdir().expect("temporary workspace");
        let root = temp.path().join("fixture");
        let include_directory = root.join("include");
        let main = root.join("Main.pas");
        let project = root.join("App.dproj");
        let include = include_directory.join("Shared.inc");
        let source = "unit Main;\ninterface\nimplementation\n{$I Shared.inc}\nprocedure Use;\nvar\n  badConst: Integer;\nbegin\n  badConst := 1;\nend;\nend.\n";
        fs::create_dir_all(&include_directory).expect("include directory");
        fs::write(&include, b"{$DEFINE FEATURE}\n").expect("resolved include");
        fs::write(&main, source).expect("main source");
        fs::write(
            &project,
            "<Project><PropertyGroup><MainSource>Main.pas</MainSource><DCC_IncludePath>include</DCC_IncludePath></PropertyGroup></Project>",
        )
        .expect("project");

        let main_uri = Url::from_file_path(&main).expect("main URI");
        let workspace = test_workspace(vec![root.clone()], WorkspaceOptions::default());
        let input = workspace.analysis_input();
        let cancel = AtomicBool::new(false);
        let computed = rename_from_input(
            input.clone(),
            &main_uri,
            Position::new(6, 2),
            "BAD_CONST",
            false,
            &cancel,
        );
        assert!(
            computed.value.is_ok(),
            "local rename with an external include must compute: {:?}",
            computed.value
        );
        revalidate_input(&input, &computed.records, &cancel)
            .expect("unchanged include lookup inputs must remain valid");

        let shadowing_include = root.join("Shared.inc");
        fs::write(&shadowing_include, b"{$DEFINE SHADOWING}\n").expect("shadowing include");
        let error = revalidate_input(&input, &computed.records, &cancel)
            .expect_err("a newly-preferred include must stale the rename plan");
        assert!(
            error.contains("changed") || error.contains("metadata"),
            "unexpected include lookup revalidation error: {error}"
        );
    }

    #[test]
    fn rename_revalidates_resolved_include_content_with_equal_metadata() {
        let temp = tempfile::tempdir().expect("temporary workspace");
        let root = temp.path().join("fixture");
        let main = root.join("Main.pas");
        let include = root.join("Shared.inc");
        let source = "unit Main;\ninterface\nimplementation\n{$I Shared.inc}\nprocedure Run;\nvar\n  badConst: Integer;\nbegin\n  badConst := 1;\nend;\nend.\n";
        let original_include = b"{$DEFINE FEATURE}\n";
        let changed_include = b"{$DEFINE CHANGED}\n";
        assert_eq!(original_include.len(), changed_include.len());
        fs::create_dir_all(&root).expect("fixture directory");
        fs::write(&include, original_include).expect("include");
        fs::write(&main, source).expect("main source");

        let main_uri = Url::from_file_path(&main).expect("main URI");
        let workspace = test_workspace(vec![root], WorkspaceOptions::default());
        let input = workspace.analysis_input();
        let validation_input = input.clone();
        let cancel = AtomicBool::new(false);
        let computed = rename_from_input(
            input,
            &main_uri,
            Position::new(6, 2),
            "BAD_CONST",
            false,
            &cancel,
        );
        let planned = computed
            .value
            .expect("rename must produce a plan before the mutation");
        assert!(
            planned
                .changes
                .as_ref()
                .and_then(|changes| changes.get(&main_uri))
                .is_some_and(|edits| !edits.is_empty()),
            "rename plan must include the target source edit"
        );

        let include_record = computed
            .records
            .iter()
            .find(|record| record.path.as_deref() == Some(include.as_path()))
            .expect("rename read-set must retain the resolved include");
        assert!(
            include_record.content_hash.is_some(),
            "resolved include must retain its content hash"
        );
        assert!(
            revalidate_input(&validation_input, &computed.records, &cancel).is_ok(),
            "unchanged rename inputs must revalidate successfully"
        );

        let original_metadata = fs::metadata(&include).expect("original include metadata");
        let original_mtime = original_metadata
            .modified()
            .expect("original include mtime");
        fs::write(&include, changed_include).expect("changed include");
        File::options()
            .write(true)
            .open(&include)
            .expect("open changed include for timestamp restore")
            .set_times(FileTimes::new().set_modified(original_mtime))
            .expect("restore include mtime");

        let changed_metadata = fs::metadata(&include).expect("changed include metadata");
        assert_eq!(changed_metadata.len(), original_metadata.len());
        assert_eq!(
            changed_metadata.modified().expect("changed include mtime"),
            original_mtime
        );
        assert_ne!(
            fs::read(&include).expect("changed include bytes"),
            original_include
        );

        let error = revalidate_input(&validation_input, &computed.records, &cancel)
            .expect_err("changed include content must invalidate the rename");
        assert!(
            error.to_ascii_lowercase().contains("changed")
                || error.to_ascii_lowercase().contains("metadata"),
            "unexpected include revalidation error: {error}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn rename_revalidates_unchanged_symlinked_include_file_and_directory() {
        let temp = tempfile::tempdir().expect("temporary workspace");
        let root = temp.path().join("fixture");
        let main = root.join("Main.pas");
        let project = root.join("App.dproj");
        let real_file = root.join("real.inc");
        let linked_file = root.join("linked.inc");
        let real_directory = root.join("real-includes");
        let linked_directory = root.join("include");
        let searched_include = real_directory.join("Shared.inc");
        let source = "unit Main;\ninterface\nconst\n  BadConst = 1;\nimplementation\n{$I linked.inc}\n{$I Shared.inc}\nend.\n";
        fs::create_dir_all(&real_directory).expect("include directory");
        fs::write(&real_file, b"{$DEFINE FILE}\n").expect("real include file");
        fs::write(&searched_include, b"{$DEFINE SEARCHED}\n").expect("searched include file");
        symlink("real.inc", &linked_file).expect("symlinked include file");
        symlink("real-includes", &linked_directory).expect("symlinked include directory");
        fs::write(&main, source).expect("main source");
        fs::write(
            &project,
            "<Project><PropertyGroup><MainSource>Main.pas</MainSource><DCC_IncludePath>include</DCC_IncludePath></PropertyGroup></Project>",
        )
        .expect("project");

        let main_uri = Url::from_file_path(&main).expect("main URI");
        let workspace = test_workspace(vec![root.clone()], WorkspaceOptions::default());
        let input = workspace.analysis_input();
        let cancel = AtomicBool::new(false);
        let computed = rename_from_input(
            input.clone(),
            &main_uri,
            Position::new(3, 2),
            "GOOD_CONST",
            false,
            &cancel,
        );
        assert!(
            computed.value.is_ok(),
            "rename must succeed with readable symlinked includes: {:?}",
            computed.value
        );
        assert!(
            computed
                .records
                .iter()
                .any(|record| { record.path.as_deref() == Some(linked_file.as_path()) }),
            "the symlinked include file must be in the revalidation read-set"
        );
        assert!(
            computed
                .records
                .iter()
                .any(|record| { record.path.as_deref() == Some(linked_directory.as_path()) }),
            "the symlinked include-search directory must be in the revalidation read-set: {:#?}",
            computed.records
        );
        assert!(
            revalidate_input(&input, &computed.records, &cancel).is_ok(),
            "unchanged readable symlinked includes must revalidate"
        );

        let changed_file = root.join("changed.inc");
        fs::write(
            &changed_file,
            b"{$DEFINE RETARGETED}\n{$DEFINE DIFFERENT}\n",
        )
        .expect("retarget include file");
        fs::remove_file(&linked_file).expect("remove old include link");
        symlink("changed.inc", &linked_file).expect("retarget include link");
        let error = revalidate_input(&input, &computed.records, &cancel)
            .expect_err("retargeted symlink content must invalidate the rename");
        assert!(
            error.contains("changed") || error.contains("metadata"),
            "unexpected symlink retarget error: {error}"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn configuration_revalidation_readers_do_not_block_on_fifo_replacement() {
        let target = std::env::var_os("LINT4D_EXACT_CONFIG_FIFO_TARGET");
        if let Some(target) = target {
            let target = std::path::PathBuf::from(target);
            let policy_root = target.parent().expect("FIFO target parent").to_path_buf();
            let read_policy = ReadPolicy::new(
                std::slice::from_ref(&policy_root),
                &[],
                &[],
                &EffectiveOverrides::default(),
            );
            let path_entry = ProjectPathEntry {
                path: target.clone(),
                provenance: ProjectPathProvenance::Configured,
            };
            let cancel = AtomicBool::new(false);
            for _ in 0..100_000 {
                let _ = read_exact_file_bytes(&target, &read_policy, &path_entry, &cancel);
                let _ = file_content_hash(&target, &read_policy, &path_entry, &cancel);
                let _ = super::read_scan_source(&target, &read_policy, &path_entry, &cancel);
            }
            return;
        }

        let temp = tempfile::tempdir().expect("temporary directory");
        let target = temp.path().join(".lint4d.toml");
        let held = temp.path().join("held-config");
        let fifo = temp.path().join("config-fifo");
        fs::write(&target, b"[rules]\n").expect("initial configuration");
        let status = Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .expect("mkfifo command");
        assert!(status.success(), "mkfifo failed");
        let mut child = Command::new(std::env::current_exe().expect("test executable"))
            .args([
                "--exact",
                "workspace::rename::tests::configuration_revalidation_readers_do_not_block_on_fifo_replacement",
                "--nocapture",
            ])
            .env("LINT4D_EXACT_CONFIG_FIFO_TARGET", &target)
            .spawn()
            .expect("spawn FIFO reader child");
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

        let deadline = Instant::now() + Duration::from_secs(5);
        let status = loop {
            if let Some(status) = child.try_wait().expect("poll FIFO reader child") {
                break status;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                stop.store(true, std::sync::atomic::Ordering::Relaxed);
                replacer.join().expect("join FIFO replacer");
                panic!("configuration revalidation reader blocked on a replaced FIFO");
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        replacer.join().expect("join FIFO replacer");
        assert!(status.success(), "FIFO reader child exited with {status}");
    }

    #[test]
    fn compiler_switch_directives_with_values_are_harmless() {
        for body in [
            "M+",
            "M-",
            "R *.dfm",
            "R-,T-,H+,X+",
            "APPTYPE CONSOLE",
            "HINTS OFF",
            "STRONGLINKTYPES OFF",
            "REGION name",
            "REGION 'section, with a comma'",
            "ENDREGION",
            "MESSAGE ERROR 'Delphi 2010 should be used to compile this.'",
            "Q-",
            "EXCESSPRECISION OFF",
            "MODE Delphi",
            "ASMMODE INTEL",
            "WARNINGS OFF",
            "WARNINGS ON",
        ] {
            assert_eq!(
                directive_kind(body),
                DirectiveKind::Harmless,
                "{body} must not make an unrelated include owner unsafe"
            );
        }
    }

    #[test]
    fn include_revalidation_requires_recorded_payload_authorization() {
        let temp = tempfile::tempdir().expect("temporary directory");
        let include = temp.path().join("Shared.inc");
        fs::write(&include, b"{$DEFINE SAFE}\n").expect("include");
        let record = super::SourceRecord {
            uri: Url::from_file_path(&include).expect("include URI"),
            text: String::new(),
            version: None,
            stamp: None,
            open: false,
            path: Some(include.clone()),
            path_stamp: None,
            content_hash: Some(1),
            parsed_text_hash: None,
            content_bytes: None,
            candidate_membership: None,
            candidate_observations: Vec::new(),
            read_policy: None,
            path_entry: None,
            include_payload: true,
            missing_provider_candidate: false,
            directory_observation: false,
            missing_provider_scope: None,
            auto_import_provider_observation: false,
            auto_import_scopes: Vec::new(),
        };

        let error = read_record_content_hash(&include, &record, &AtomicBool::new(false))
            .expect_err("unbound include records must not perform a bare revalidation read");
        assert!(
            error.contains("requester-scoped") || error.contains("authorization"),
            "unexpected missing authorization error: {error}"
        );
    }
}
