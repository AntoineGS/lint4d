//! Complete, isolated workspace snapshots used by rename and code actions.
//!
//! Navigation intentionally loads a small dependency closure.  Rename cannot
//! use that closure: an unopened reverse consumer may be anywhere in the
//! selected workspace.  This module therefore builds a bounded, throw-away
//! index for each expensive request and never mutates the live navigation
//! cache or the filesystem.

use super::{
    ContextKey, ContextState, DiskStamp, KnownDocumentOwner, OpenDocument, PathStamp, Workspace,
    WorkspaceOptions, absolute_path, canonical_file_uri, disk_stamp, is_configuration_file,
    is_pascal_path, path_stamp, path_stamp_result, path_starts_with_ci, path_starts_with_native,
    paths_equal_ci, read_disk_source,
};
use crate::NavigationIndex;
use crate::conditional::{self, ConditionalDirective, DirectiveKind as ConditionalDirectiveKind};
use crate::project::{
    MetadataObservation, ProjectCandidateMembership, ProjectContext, ProjectPathEntry,
    ProjectPathProvenance, ProjectSelections, ReadPolicy, has_invalid_project_selection,
};
use crate::text;
use lsp_types::{
    DocumentChanges, OneOf, OptionalVersionedTextDocumentIdentifier, Position,
    PrepareRenameResponse, TextDocumentEdit, TextEdit, Url, WorkspaceEdit,
};
use pascal_core::decode_bytes;
use pascal_core::delphi_overrides::EffectiveOverrides;
#[cfg(test)]
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use walkdir::WalkDir;

#[cfg(test)]
use std::sync::mpsc::{Receiver, Sender};

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
const MAX_RENAME_INCLUDE_DEPTH: usize = 256;
const MAX_RENAME_INCLUDE_OWNER_SUMMARY_BYTES: usize = 64 * 1024;
const MAX_RENAME_CONFIG_BYTES: usize = 4 * 1024 * 1024;
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
pub(crate) struct WorkspaceInput {
    pub(crate) roots: Vec<PathBuf>,
    pub(crate) options: WorkspaceOptions,
    pub(crate) overrides: pascal_core::delphi_overrides::OverrideSession,
    pub(crate) project_selections: ProjectSelections,
    pub(crate) document_owners: HashMap<Url, KnownDocumentOwner>,
    pub(crate) overlays: HashMap<Url, OverlayInput>,
    pub(crate) rejected_documents: HashSet<Url>,
    pub(crate) source_generation: u64,
    pub(crate) configuration_generation: u64,
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
    pub(crate) content_bytes: Option<Vec<u8>>,
    pub(crate) candidate_membership: Option<ProjectCandidateMembership>,
    /// The requester-scoped authorization used to read this closed source.
    /// Open overlays do not need these values because their payload is already
    /// supplied by the client and revalidation compares the overlay text.
    pub(crate) read_policy: Option<ReadPolicy>,
    pub(crate) path_entry: Option<ProjectPathEntry>,
    pub(crate) include_payload: bool,
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
}

impl SnapshotSeed {
    pub(crate) fn new(record: SourceRecord) -> Self {
        Self {
            record,
            consumed_configuration: Vec::new(),
        }
    }

    pub(crate) fn with_consumed_configuration(mut self, records: &[SourceRecord]) -> Self {
        self.consumed_configuration = records.to_vec();
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
    pub(crate) readable: HashSet<Url>,
    pub(crate) editable: HashSet<Url>,
    pub(crate) complete: bool,
    pub(crate) incomplete_reason: Option<String>,
    pub(crate) include_errors: Vec<String>,
    pub(crate) baseline_records: Vec<SourceRecord>,
    pub(crate) mode: SnapshotMode,
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
type SnapshotPriorityBarrier = (Url, Sender<()>, Receiver<()>);

#[cfg(test)]
thread_local! {
    static SNAPSHOT_PRIORITY_BARRIER: RefCell<Option<SnapshotPriorityBarrier>> =
        const { RefCell::new(None) };
}

#[cfg(test)]
fn install_snapshot_priority_barrier(priority_uri: Url, ready: Sender<()>, release: Receiver<()>) {
    SNAPSHOT_PRIORITY_BARRIER
        .with(|barrier| barrier.borrow_mut().replace((priority_uri, ready, release)));
}

#[cfg(test)]
fn wait_at_snapshot_priority_barrier(priority: &[Url]) {
    let Some(priority_uri) = priority.first() else {
        return;
    };
    let barrier = SNAPSHOT_PRIORITY_BARRIER.with(|barrier| {
        let mut barrier = barrier.borrow_mut();
        barrier
            .as_ref()
            .is_some_and(|(expected, _, _)| expected == priority_uri)
            .then(|| barrier.take().expect("snapshot barrier is present"))
    });
    if let Some((_, ready, release)) = barrier {
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
        WorkspaceInput {
            roots: self.roots.iter().map(|root| root.path.clone()).collect(),
            options: self.options.clone(),
            overrides: self.overrides.clone(),
            project_selections: self.project_selections.clone(),
            document_owners: self.document_owners.clone(),
            overlays,
            rejected_documents,
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
                revalidate_path_record(path, record, &cancel)?;
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
                        document
                            .text
                            .as_deref()
                            .is_none_or(|text| text_content_hash(text) != expected)
                    },
                );
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
            if current.text != record.text
                || record
                    .content_hash
                    .is_some_and(|expected| expected != current.content_hash)
            {
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
    for record in records {
        if is_cancelled(cancel) {
            return Err(CANCELLATION_MESSAGE.to_string());
        }
        if let Some(path) = &record.path {
            revalidate_path_record(path, record, cancel)?;
            continue;
        }
        if record.open {
            let Some(overlay) = input.overlays.get(&record.uri) else {
                return Err(format!(
                    "open document disappeared while resolving {}; retry the request",
                    record.uri
                ));
            };
            let text_changed = record.content_hash.map_or_else(
                || overlay.text != record.text,
                |expected| text_content_hash(&overlay.text) != expected,
            );
            if overlay.version != record.version.unwrap_or_default() || text_changed {
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
            input.options.limits.max_file_bytes,
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
        if disk_stamp(&path) != record.stamp
            || current.text != record.text
            || record
                .content_hash
                .is_some_and(|expected| expected != current.content_hash)
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
) -> Result<(), String> {
    if is_cancelled(cancel) {
        return Err(CANCELLATION_MESSAGE.to_string());
    }
    if let Some(expected) = &record.candidate_membership {
        let actual =
            crate::project::project_candidate_membership(path, Some(cancel)).map_err(|error| {
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
        if actual_path_stamp != record.path_stamp {
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

fn text_content_hash(source: &str) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    source.hash(&mut hasher);
    hasher.finish()
}

#[derive(Debug)]
struct ScannedSource {
    data: Vec<u8>,
    bytes: usize,
    content_hash: u64,
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
        content_bytes,
        candidate_membership,
        read_policy,
        path_entry,
        include_payload,
    })
}

pub(crate) fn snapshot_records(snapshot: &RenameSnapshot) -> Vec<SourceRecord> {
    let mut records = snapshot.records.values().cloned().collect::<Vec<_>>();
    records.extend(snapshot.baseline_records.iter().cloned());
    records
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
                content_bytes: None,
                candidate_membership: None,
                read_policy: None,
                path_entry: None,
                include_payload: false,
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
                content_bytes: None,
                candidate_membership: None,
                read_policy: None,
                path_entry: None,
                include_payload: false,
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
        content_bytes: None,
        candidate_membership: None,
        read_policy: Some(read_policy),
        path_entry: Some(entry),
        include_payload: false,
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

pub(crate) fn owner_for_input(
    input: &WorkspaceInput,
    uri: &Url,
    cancel: &AtomicBool,
) -> Result<KnownDocumentOwner, String> {
    let uri = canonical_file_uri(uri);
    let owner_origin = input.document_owners.get(&uri).map(|owner| owner.origin);

    let mut workspace = Workspace::with_override_session(
        input.roots.clone(),
        input.options.clone(),
        input.overrides.clone(),
    );
    workspace.project_selections = input.project_selections.clone();
    workspace.document_owners = input.document_owners.clone();
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
    let defines: &[String] =
        if !context.discovery_complete && !matches!(self_contained_mode, SelfContainedMode::None) {
            &[]
        } else {
            &context.defines
        };
    let (info, ignored_or_empty) = binding_info_for_source(
        uri,
        &source,
        position,
        additional_names,
        defines,
        self_contained_mode,
        cancel,
    )?;
    Ok(BindingClassification {
        source,
        record,
        info,
        ignored_or_empty,
        consumed_configuration,
    })
}

pub(crate) fn project_context_and_metadata_for_input(
    input: &WorkspaceInput,
    uri: &Url,
    cancel: &AtomicBool,
) -> Result<(ProjectContext, Vec<SourceRecord>), String> {
    let mut workspace = Workspace::with_override_session(
        input.roots.clone(),
        input.options.clone(),
        input.overrides.clone(),
    );
    workspace.project_selections = input.project_selections.clone();
    workspace.document_owners = input.document_owners.clone();
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

fn consumed_context_records(
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
        if let Some(record) = path_record_at(
            path,
            stamp,
            content_hash,
            content_bytes,
            None,
            read_policy,
            path_entry,
            false,
        ) {
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
    defines: &[String],
    self_contained_mode: SelfContainedMode,
    cancel: &AtomicBool,
) -> Result<(Option<(crate::navigation::RenameBindingInfo, bool)>, bool), String> {
    let uri = canonical_file_uri(uri);
    let mut index = NavigationIndex::new();
    index
        .update_with_defines_with_cancel(uri.clone(), source.to_owned(), defines, cancel)
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

fn may_contain_include_directive(source: &[u8]) -> bool {
    source.windows(2).any(|window| window == b"{$")
        || source.windows(3).any(|window| window == b"(*$")
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
    let (mode, candidate_names, self_contained) = match binding_info {
        Some((info, self_contained)) => {
            let mut names = info.names;
            if names.is_empty() {
                names.push(original_name.clone());
            }
            (
                if info.local {
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
    let value = snapshot.index.prepare_rename(&uri, position);
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
    let (mode, candidate_names, self_contained) = match binding_info {
        Some((info, self_contained)) => {
            let mut names = info.names;
            if names.is_empty() {
                names.push(original_name.clone());
            }
            names.push(new_name.to_string());
            (
                if info.local {
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

    let raw_edits = match snapshot.index.rename_edits(&uri, position, new_name) {
        Ok(edits) => edits,
        Err(error) => {
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
    if let Some(owner) = loader.document_owners.get(uri).cloned()
        && (loader.context_has_open_legacy_overlay(&owner.state)
            || (!super::context_state_is_fresh_with_cancel(&owner.state, Some(cancel))?
                && loader
                    .context_state_is_fresh_with_open_documents(&owner.state, Some(cancel))?))
    {
        loader
            .contexts
            .insert(owner.key.clone(), owner.state.clone());
        loader
            .document_contexts
            .insert(uri.clone(), owner.key.clone());
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

    let mut add_entry = |entry: &crate::project::ProjectPathEntry| match &entry.provenance {
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
        if !readable || !is_pascal_path(&path) {
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
    loader.project_selections = input.project_selections.clone();
    loader.document_owners = input.document_owners.clone();
    for (uri, overlay) in &input.overlays {
        loader.open_documents.insert(
            uri.clone(),
            OpenDocument {
                text: Some(overlay.text.clone()),
                version: overlay.version,
                rejection: None,
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
                return Err(format!(
                    "rename workspace scan incomplete: project context is ambiguous or incomplete for {uri}"
                ));
            }
        }
    }
    let mut enumeration = enumerate_sources(&loader, input, &priority, mode, cancel)?;
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
    let paths = enumeration.paths;
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
    let mut indexed_sizes = HashMap::new();
    let mut indexed_stamps = HashMap::new();
    let mut indexed_uris = HashSet::new();
    let mut retained_files = 0usize;
    let mut retained_bytes = 0usize;
    let mut scanned_bytes = 0usize;
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
                    content_bytes: None,
                    candidate_membership: None,
                    read_policy: Some(read_policy.clone()),
                    path_entry: Some(path_entry.clone()),
                    include_payload: false,
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
            baseline_content_hashes
                .entry(path_key(&path))
                .or_insert(scan.content_hash);
            baseline.set_payload_dependency(&path, read_policy.clone(), path_entry.clone());
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
            let direct_candidate = candidate_names.is_empty()
                || !candidate_names_are_ascii
                || contains_any_identifier_bytes(&scan.data, candidate_names);
            if !is_priority && !direct_candidate && !may_contain_include_directive(&scan.data) {
                continue;
            }
            let source = decode_bytes(&scan.data).into_owned();
            let stamp = disk_stamp(&path)
                .ok_or_else(|| format!("rename workspace scan could not stat source {path:?}"))?;
            (
                source.clone(),
                SourceRecord {
                    uri: uri.clone(),
                    text: source,
                    version: None,
                    stamp: Some(stamp),
                    open: false,
                    path: None,
                    path_stamp: None,
                    content_hash: Some(scan.content_hash),
                    content_bytes: None,
                    candidate_membership: None,
                    read_policy: Some(read_policy.clone()),
                    path_entry: Some(path_entry.clone()),
                    include_payload: false,
                },
                scan.bytes,
            )
        };
        if !record.open {
            baseline.set_payload_dependency(&path, read_policy.clone(), path_entry.clone());
        }
        if let Some(content_hash) = record.content_hash {
            baseline_content_hashes
                .entry(path_key(&path))
                .or_insert(content_hash);
        }
        let should_index = is_priority
            || candidate_names.is_empty()
            || contains_any_identifier(&source, candidate_names);
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
        let defines = contexts
            .get(&uri)
            .and_then(|context_key| loader.contexts.get(context_key))
            .map(|state| {
                if allow_incomplete_context_for
                    .iter()
                    .any(|allowed_uri| allowed_uri == &uri)
                    && !state.context.discovery_complete
                {
                    Vec::new()
                } else {
                    state.context.defines.clone()
                }
            })
            .unwrap_or_default();
        index
            .update_with_defines_with_cancel(uri.clone(), source.clone(), &defines, cancel)
            .map_err(|error| format!("rename workspace scan could not index {uri}: {error}"))?;
        retained_files = retained_files.saturating_add(1);
        retained_bytes = retained_bytes.saturating_add(source_bytes);
        indexed_uris.insert(uri.clone());
        indexed_sizes.insert(uri.clone(), source.len());
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
    for (uri, context_key) in &contexts {
        loader
            .document_contexts
            .insert(uri.clone(), context_key.clone());
    }
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
                let import_count = loader.index.imports(&uri).len();
                let dependencies =
                    loader.load_imports_with_cancel(&uri, &context_key, &mut pins, Some(cancel))?;
                if dependencies.len() < import_count {
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
                    content_bytes: None,
                    candidate_membership: None,
                    read_policy: None,
                    path_entry: None,
                    include_payload: false,
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
                    text: source,
                    version: None,
                    stamp: Some(stamp),
                    open: false,
                    path: None,
                    path_stamp: None,
                    content_hash: None,
                    content_bytes: None,
                    candidate_membership: None,
                    read_policy: None,
                    path_entry: None,
                    include_payload: false,
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
    let baseline_records = baseline
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

    Ok(RenameSnapshot {
        index: loader.index,
        sources,
        records,
        readable,
        editable,
        complete,
        incomplete_reason,
        include_errors,
        baseline_records,
        mode,
    })
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
        return Err(format!("rename workspace scan incomplete: {reason}"));
    }
    Ok(())
}

fn enumerate_sources(
    workspace: &Workspace,
    input: &WorkspaceInput,
    priority: &[Url],
    mode: SnapshotMode,
    cancel: &AtomicBool,
) -> Result<Enumeration, String> {
    let mut result = Enumeration {
        complete: true,
        ..Enumeration::default()
    };
    if matches!(
        mode,
        SnapshotMode::Local | SnapshotMode::LocalWithImports | SnapshotMode::Assistance
    ) {
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
                observe_directory_stamps,
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
                crate::project::MetadataObservation::Stat { .. } => {
                    add_baseline_path_with_stamp(baseline, path.to_path_buf(), path_stamp(path));
                }
                crate::project::MetadataObservation::Payload {
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
}

impl IncludeAnalysis {
    fn safe(relevant: bool) -> Self {
        Self {
            safe: true,
            relevant,
            reason: None,
        }
    }

    fn unsafe_with_reason(reason: impl Into<String>, relevant: bool) -> Self {
        Self {
            safe: false,
            relevant,
            reason: Some(reason.into()),
        }
    }
}

struct IncludeAuditor<'a> {
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
    selected_directory: Option<&'a Path>,
    relative: bool,
    route: IncludeRoute,
    legacy_authorized: bool,
}

fn audit_includes(mut auditor: IncludeAuditor<'_>) -> Result<IncludeAuditResult, String> {
    let mut uris = auditor.sources.keys().cloned().collect::<Vec<_>>();
    uris.sort_by(|left, right| left.as_str().cmp(right.as_str()));
    for uri in uris {
        if auditor.stopped {
            break;
        }
        if is_cancelled(auditor.cancel) {
            return Err(CANCELLATION_MESSAGE.to_string());
        }
        let context = auditor.context_for_source(&uri)?;
        let Some(source) = auditor.sources.get(&uri) else {
            continue;
        };
        let defines = context
            .as_ref()
            .map(|(_, context)| {
                if !context.discovery_complete
                    && auditor
                        .allow_incomplete_context_for
                        .iter()
                        .any(|allowed_uri| allowed_uri == &uri)
                {
                    Vec::new()
                } else {
                    context.defines.clone()
                }
            })
            .unwrap_or_default();
        #[cfg(test)]
        if TEST_CANCEL_INCLUDE_ANALYSIS.with(Cell::get) {
            auditor.cancel.store(true, Ordering::Relaxed);
        }
        let conditional = conditional::analyze_with_cancel(source, &defines, auditor.cancel);
        if is_cancelled(auditor.cancel) {
            return Err(CANCELLATION_MESSAGE.to_string());
        }
        if !conditional.complete {
            auditor.result.incomplete_reason.get_or_insert_with(|| {
                format!("conditional analysis is incomplete for include owner {uri}")
            });
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
        let include_directives = conditional
            .directives
            .into_iter()
            .filter(|directive| {
                directive.kind == ConditionalDirectiveKind::Include
                    && directive.potentially_active()
            })
            .map(|directive| legacy_directive(&directive))
            .collect::<Vec<_>>();
        if include_directives.is_empty() {
            continue;
        }

        let incomplete_context = context
            .as_ref()
            .is_none_or(|(_, context)| !context.discovery_complete);
        let allow_incomplete_context = auditor
            .allow_incomplete_context_for
            .iter()
            .any(|allowed_uri| allowed_uri == &uri);
        if incomplete_context && allow_incomplete_context {
            auditor.record_error(format!(
                "rename cannot prove completeness because potentially active include in {uri} depends on incomplete project search paths"
            ));
            auditor.stopped = true;
            break;
        }

        let context = auditor.context_for_source(&uri)?;
        for directive in include_directives {
            if auditor.stopped {
                break;
            }
            if is_cancelled(auditor.cancel) {
                return Err(CANCELLATION_MESSAGE.to_string());
            }
            if !auditor.take_directive_budget() {
                auditor.record_error(format!(
                    "rename cannot prove completeness because include directive limit ({MAX_RENAME_INCLUDE_DIRECTIVES}) was reached"
                ));
                auditor.stopped = true;
                break;
            }
            let Some((context_key, context)) = context.as_ref() else {
                auditor.record_error(format!(
                    "rename cannot prove completeness because include owner {uri} has no project context"
                ));
                auditor.stopped = true;
                break;
            };
            auditor.inspect_top_level(&uri, &directive, context_key, context)?;
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

    fn inspect_top_level(
        &mut self,
        uri: &Url,
        directive: &Directive,
        context_key: &ContextKey,
        context: &ProjectContext,
    ) -> Result<(), String> {
        let Some(owner_path) = uri.to_file_path().ok().map(absolute_path) else {
            self.record_error(format!(
                "rename cannot prove completeness because an include path in {uri} is unresolved"
            ));
            self.stopped = true;
            return Ok(());
        };

        let directories = include_search_directories(&owner_path, Some(context));
        let lookup =
            resolve_include_path_with_overrides(directive, &directories, &context.overrides);
        self.observe_lookup(&lookup);
        if let Some(error) = lookup.error {
            self.record_error(format!(
                "rename cannot prove completeness because include in {uri} could not be read: {error}"
            ));
            self.stopped = true;
            return Ok(());
        }
        let Some(path) = lookup.selected else {
            self.record_error(format!(
                "rename cannot prove completeness because an include path in {uri} is unresolved"
            ));
            self.stopped = true;
            return Ok(());
        };

        let route = lookup.selected_route.clone();
        let legacy_authorized = matches!(&route, IncludeRoute::Legacy)
            && legacy_include_is_authorized(
                self.loader,
                context,
                &owner_path,
                lookup.selected_directory.as_deref(),
                include_name(directive).is_some_and(|raw| Path::new(&raw).is_relative()),
            );
        let inspection = IncludeInspection {
            context_key,
            context,
            owner_path: &owner_path,
            selected_directory: lookup.selected_directory.as_deref(),
            relative: include_name(directive).is_some_and(|raw| Path::new(&raw).is_relative()),
            route,
            legacy_authorized,
        };
        let analysis = self.inspect_include_file(&path, inspection, 0)?;
        if !analysis.safe || analysis.relevant {
            let reason = analysis
                .reason
                .unwrap_or_else(|| format!("include {path:?} contains source content"));
            self.record_error(format!("rename cannot prove completeness because {reason}"));
            self.stopped = true;
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn inspect_nested(
        &mut self,
        owner_path: &Path,
        directive: &Directive,
        context_key: &ContextKey,
        context: &ProjectContext,
        inherited_route: IncludeRoute,
        legacy_authorized: bool,
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
        let directories = include_search_directories(owner_path, Some(context));
        let lookup =
            resolve_include_path_with_overrides(directive, &directories, &context.overrides);
        self.observe_lookup(&lookup);
        if let Some(error) = lookup.error {
            return Ok(IncludeAnalysis::unsafe_with_reason(
                format!("include in {owner_path:?} could not be read: {error}"),
                false,
            ));
        }
        let Some(path) = lookup.selected else {
            return Ok(IncludeAnalysis::unsafe_with_reason(
                format!("include in {owner_path:?} has an unresolved path"),
                false,
            ));
        };
        let route = match lookup.selected_route {
            IncludeRoute::Mapped { root } => IncludeRoute::Mapped { root },
            IncludeRoute::Legacy => inherited_route,
        };
        let legacy_authorized = matches!(&route, IncludeRoute::Legacy)
            && inherited_legacy_authorization(
                legacy_authorized,
                owner_path,
                directive,
                lookup.selected_directory.as_deref(),
                context,
            );
        let inspection = IncludeInspection {
            context_key,
            context,
            owner_path,
            selected_directory: lookup.selected_directory.as_deref(),
            relative: include_name(directive).is_some_and(|raw| Path::new(&raw).is_relative()),
            route,
            legacy_authorized,
        };
        self.inspect_include_file(&path, inspection, depth + 1)
    }

    fn inspect_include_file(
        &mut self,
        path: &Path,
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
                    "include {path:?} exceeds the maximum include nesting depth ({MAX_RENAME_INCLUDE_DEPTH})"
                ),
                false,
            ));
        }
        let directories = include_search_directories(path, Some(inspection.context));
        let cache_key = include_cache_key(
            path,
            &directories,
            inspection.context_key,
            inspection.owner_path,
            inspection.selected_directory,
            inspection.relative,
            &inspection.route,
            inspection.legacy_authorized,
        );
        let active_key = canonical_include_key(path);
        if self.active.contains(&active_key) {
            return Ok(IncludeAnalysis::unsafe_with_reason(
                format!("include {path:?} has an include cycle"),
                false,
            ));
        }
        if let Some(analysis) = self.cache.get(&cache_key) {
            return Ok(analysis.clone());
        }
        if !is_readable_include_for_context(
            self.loader,
            path,
            inspection.context_key,
            inspection.context,
            &inspection.route,
            inspection.legacy_authorized,
        ) {
            let analysis = IncludeAnalysis::unsafe_with_reason(
                format!("include {path:?} is outside the owning project's readable roots"),
                false,
            );
            self.cache.insert(cache_key, analysis.clone());
            return Ok(analysis);
        }
        let path_entry = match &inspection.route {
            IncludeRoute::Mapped { root } => Some(ProjectPathEntry {
                path: path.to_path_buf(),
                provenance: ProjectPathProvenance::Mapped { root: root.clone() },
            }),
            IncludeRoute::Legacy => {
                super::context_path_entry(inspection.context, path).or_else(|| {
                    inspection.legacy_authorized.then(|| ProjectPathEntry {
                        path: path.to_path_buf(),
                        provenance: ProjectPathProvenance::LegacyNative,
                    })
                })
            }
        };
        let Some(path_entry) = path_entry else {
            let analysis = IncludeAnalysis::unsafe_with_reason(
                format!("include {path:?} has no requester-scoped read authorization"),
                false,
            );
            self.cache.insert(cache_key, analysis.clone());
            return Ok(analysis);
        };
        if matches!(&inspection.route, IncludeRoute::Mapped { .. })
            && !matches!(&path_entry.provenance, ProjectPathProvenance::Mapped { .. })
        {
            let analysis = IncludeAnalysis::unsafe_with_reason(
                format!("mapped include {path:?} has no mapped provenance"),
                false,
            );
            self.cache.insert(cache_key, analysis.clone());
            return Ok(analysis);
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
        let include_source = match if matches!(&inspection.route, IncludeRoute::Mapped { .. }) {
            read_mapped_include(
                &inspection.context.read_policy,
                &path_entry,
                self.max_file_bytes,
                remaining_bytes,
                self.cancel,
            )
        } else {
            read_include(
                path,
                &inspection.context.read_policy,
                &path_entry,
                self.max_file_bytes,
                Some(remaining_bytes),
                Some(self.cancel),
            )
        } {
            Ok(source) => source,
            Err(error) if error == CANCELLATION_MESSAGE => return Err(error),
            Err(error) => {
                if error == INCLUDE_BYTE_BUDGET_ERROR {
                    self.stopped = true;
                }
                let analysis = IncludeAnalysis::unsafe_with_reason(
                    format!("include {path:?} could not be read: {error}"),
                    false,
                );
                self.cache.insert(cache_key, analysis.clone());
                return Ok(analysis);
            }
        };
        self.bytes_read = self.bytes_read.saturating_add(include_source.bytes);
        self.baseline_content_hashes
            .entry(path_key(path))
            .or_insert(include_source.content_hash);
        self.baseline.set_include_payload_dependency(
            path,
            inspection.context.read_policy.clone(),
            path_entry,
        );

        // The caller's project defines are not necessarily the state at this
        // include boundary: the owner may have DEFINE/UNDEF directives, and
        // preceding includes may have changed the environment. Until the
        // auditor carries that state soundly, start include analysis with
        // unknown facts rather than resurrecting stale project facts.
        let conditional = conditional::analyze_with_cancel(&include_source.text, &[], self.cancel);
        if is_cancelled(self.cancel) {
            return Err(CANCELLATION_MESSAGE.to_string());
        }
        let relevant = if self.name_free_assistance {
            projected_source_contains_pascal_tokens(&conditional.projected_source)
        } else {
            conditional
                .potentially_active_contains_identifier(&include_source.text, self.candidate_names)
                || conditional.pascal_condition_contains_identifier(self.candidate_names)
        };
        let include_directives = conditional
            .directives
            .iter()
            .filter(|directive| {
                directive.kind == ConditionalDirectiveKind::Include
                    && directive.potentially_active()
            })
            .map(legacy_directive)
            .collect::<Vec<_>>();
        let analysis = if self.bytes_read >= MAX_RENAME_INCLUDE_BYTES {
            self.stopped = true;
            IncludeAnalysis::unsafe_with_reason(
                format!(
                    "include {path:?} could not be audited because include byte limit ({MAX_RENAME_INCLUDE_BYTES}) was reached"
                ),
                relevant,
            )
        } else if !conditional.complete {
            IncludeAnalysis::unsafe_with_reason(
                format!("include {path:?} has malformed or incomplete conditional directives"),
                relevant,
            )
        } else if conditional.directives.iter().any(|directive| {
            directive.potentially_active() && directive.kind == ConditionalDirectiveKind::Other
        }) {
            IncludeAnalysis::unsafe_with_reason(
                format!("include {path:?} contains an unsupported directive"),
                relevant,
            )
        } else {
            self.active.insert(active_key.clone());
            let mut nested = None;
            for directive in include_directives {
                if is_cancelled(self.cancel) {
                    self.active.remove(&active_key);
                    return Err(CANCELLATION_MESSAGE.to_string());
                }
                if !self.take_directive_budget() {
                    self.stopped = true;
                    nested = Some(IncludeAnalysis::unsafe_with_reason(
                        format!(
                            "include {path:?} could not be audited because include directive limit ({MAX_RENAME_INCLUDE_DIRECTIVES}) was reached"
                        ),
                        relevant,
                    ));
                    break;
                }
                if directive.kind != DirectiveKind::Include {
                    continue;
                }
                let child = self.inspect_nested(
                    path,
                    &directive,
                    inspection.context_key,
                    inspection.context,
                    inspection.route.clone(),
                    inspection.legacy_authorized,
                    depth,
                )?;
                if !child.safe || child.relevant {
                    self.stopped = true;
                    nested = Some(IncludeAnalysis::unsafe_with_reason(
                        child
                            .reason
                            .unwrap_or_else(|| format!("include {path:?} contains source content")),
                        child.relevant,
                    ));
                    break;
                }
            }
            self.active.remove(&active_key);
            nested.unwrap_or_else(|| {
                if relevant {
                    IncludeAnalysis::unsafe_with_reason(
                        format!("include {path:?} contains source content"),
                        true,
                    )
                } else {
                    IncludeAnalysis::safe(false)
                }
            })
        };
        self.cache.insert(cache_key, analysis.clone());
        Ok(analysis)
    }

    fn observe_lookup(&mut self, lookup: &IncludeLookup) {
        for observation in &lookup.observations {
            add_baseline_path_with_stamp(
                self.baseline,
                observation.path.clone(),
                observation.stamp.clone(),
            );
        }
    }

    fn record_error(&mut self, error: String) {
        if self.result.errors.len() < MAX_RENAME_INCLUDE_ERRORS {
            self.result.errors.push(error);
        } else if self.result.errors.len() == MAX_RENAME_INCLUDE_ERRORS {
            self.result.errors.push(format!(
                "rename cannot prove completeness because include error limit ({MAX_RENAME_INCLUDE_ERRORS}) was reached"
            ));
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

#[allow(clippy::too_many_arguments)]
fn include_cache_key(
    path: &Path,
    directories: &[PathBuf],
    context_key: &ContextKey,
    owner_path: &Path,
    selected_directory: Option<&Path>,
    relative: bool,
    route: &IncludeRoute,
    legacy_authorized: bool,
) -> String {
    let mut hasher = DefaultHasher::new();
    path_key(path).hash(&mut hasher);
    for directory in directories {
        path_key(directory).hash(&mut hasher);
    }
    context_key.hash(&mut hasher);
    path_key(owner_path).hash(&mut hasher);
    selected_directory.map(path_key).hash(&mut hasher);
    relative.hash(&mut hasher);
    route.hash(&mut hasher);
    legacy_authorized.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
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

fn owner_has_legacy_or_workspace_authority(
    workspace: &Workspace,
    owner_path: &Path,
    context: &ProjectContext,
) -> bool {
    if workspace.accepts_path(owner_path) {
        return true;
    }
    let is_legacy = |entry: &crate::project::ProjectPathEntry| {
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
fn resolve_include_path(directive: &Directive, directories: &[PathBuf]) -> IncludeLookup {
    resolve_include_path_with_overrides(directive, directories, &EffectiveOverrides::default())
}

fn resolve_include_path_with_overrides(
    directive: &Directive,
    directories: &[PathBuf],
    overrides: &EffectiveOverrides,
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
    bytes: usize,
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
    let (read_policy, path_entry) = record.payload_dependency()?;
    file_content_hash(path, read_policy, path_entry, cancel)
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
        BaselineAccumulator, ContextKey, ContextState, Directive, DirectiveKind, Enumeration,
        IncludeAuditResult, IncludeAuditor, IncludeInspection, IncludeRoute, PathStamp,
        ProjectCandidateMembership, ProjectContext, ProjectPathEntry, ProjectPathProvenance,
        ReadPolicy, SnapshotMode, Workspace, WorkspaceOptions, build_snapshot,
        capture_consumed_configuration_baseline, capture_context_baseline, contains_any_identifier,
        directive_kind, enumerate_external_overlays, file_content_hash,
        install_snapshot_priority_barrier, path_key, path_record_at, read_exact_file_bytes,
        read_include, read_record_content_hash, rename_from_input, resolve_include_path,
        resolve_include_path_with_overrides, revalidate_input, snapshot_records,
        test_cancel_in_include_analysis,
    };
    use lsp_types::{Position, Url};
    use pascal_core::delphi_overrides::{EffectiveOverrides, OverrideSession, PathMapping};
    use std::collections::{HashMap, HashSet};
    use std::fs::{self, File, FileTimes};
    #[cfg(unix)]
    use std::os::unix::fs::symlink;
    use std::path::PathBuf;
    #[cfg(target_os = "linux")]
    use std::process::Command;
    use std::sync::atomic::AtomicBool;
    use std::sync::mpsc;
    use std::thread;
    #[cfg(target_os = "linux")]
    use std::time::{Duration, Instant};

    fn test_workspace(roots: Vec<PathBuf>, options: WorkspaceOptions) -> Workspace {
        Workspace::with_override_session(roots, options, OverrideSession::new(None))
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
            "the symlinked include-search directory must be in the revalidation read-set"
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
                let _ = read_include(
                    &target,
                    &read_policy,
                    &path_entry,
                    16 * 1024 * 1024,
                    None,
                    None,
                );
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
    fn include_cache_does_not_share_authorized_and_unauthorized_routes() {
        let temp = tempfile::tempdir().expect("temporary directory");
        let root = temp.path().join("project");
        let main = root.join("Main.pas");
        let outside = temp.path().join("outside/Shared.inc");
        fs::create_dir_all(&root).expect("project directory");
        fs::create_dir_all(outside.parent().expect("include parent")).expect("include directory");
        fs::write(&main, "unit Main; interface implementation end.\n").expect("main source");
        fs::write(&outside, "{$DEFINE SAFE}\n").expect("include source");

        let main_uri = Url::from_file_path(&main).expect("main URI");
        let mut workspace = test_workspace(vec![root.clone()], WorkspaceOptions::default());
        let context_key = workspace.context_for_uri(&main_uri).expect("main context");
        let context = workspace
            .contexts
            .get(&context_key)
            .expect("retained main context")
            .context
            .clone();
        let sources = HashMap::new();
        let mut contexts = HashMap::new();
        let mut baseline = BaselineAccumulator::default();
        let mut baseline_content_hashes = HashMap::new();
        let mut baseline_contents = HashMap::new();
        let candidate_names = Vec::new();
        let cancel = AtomicBool::new(false);
        let mut auditor = IncludeAuditor {
            sources: &sources,
            loader: &mut workspace,
            contexts: &mut contexts,
            baseline: &mut baseline,
            baseline_content_hashes: &mut baseline_content_hashes,
            baseline_contents: &mut baseline_contents,
            candidate_names: &candidate_names,
            max_file_bytes: 1024,
            observe_directory_stamps: false,
            allow_incomplete_context_for: &[],
            cancel: &cancel,
            cache: HashMap::new(),
            active: HashSet::new(),
            files_read: 0,
            bytes_read: 0,
            directives_seen: 0,
            stopped: false,
            result: IncludeAuditResult::default(),
            name_free_assistance: false,
        };

        let allowed = auditor
            .inspect_include_file(
                &outside,
                IncludeInspection {
                    context_key: &context_key,
                    context: &context,
                    owner_path: &main,
                    selected_directory: Some(root.as_path()),
                    relative: true,
                    route: IncludeRoute::Legacy,
                    legacy_authorized: true,
                },
                0,
            )
            .expect("authorized route");
        assert!(allowed.safe, "the authorized route must be accepted");

        let denied = auditor
            .inspect_include_file(
                &outside,
                IncludeInspection {
                    context_key: &context_key,
                    context: &context,
                    owner_path: &main,
                    selected_directory: Some(root.as_path()),
                    relative: true,
                    route: IncludeRoute::Legacy,
                    legacy_authorized: false,
                },
                0,
            )
            .expect("unauthorized route");
        assert!(
            !denied.safe,
            "the unauthorized route must not reuse the allow result"
        );
    }

    #[test]
    fn include_lookup_keeps_the_missing_precedence_observation() {
        let temp = tempfile::tempdir().expect("temporary directory");
        let root = temp.path().join("root");
        let fallback = temp.path().join("fallback");
        let earlier = root.join("subdir/Shared.inc");
        let selected = fallback.join("subdir/Shared.inc");
        fs::create_dir_all(earlier.parent().expect("earlier parent")).expect("earlier directory");
        fs::create_dir_all(selected.parent().expect("selected parent"))
            .expect("selected directory");
        fs::write(&selected, "{$DEFINE SAFE}\n").expect("selected include");

        let directive = Directive {
            kind: DirectiveKind::Include,
            body: "I subdir/Shared.inc".to_string(),
            start: 0,
            end: 0,
        };
        let lookup = resolve_include_path(&directive, &[root, fallback]);
        assert_eq!(lookup.selected.as_deref(), Some(selected.as_path()));
        let observation = lookup
            .observations
            .iter()
            .find(|observation| observation.path == earlier)
            .expect("earlier candidate observation");
        assert!(observation.stamp.is_none());

        fs::write(&earlier, "procedure Hidden; begin end;\n").expect("create earlier include");
        assert!(observation.stamp.is_none());
    }

    #[test]
    fn include_lookup_matches_pascal_file_names_case_insensitively() {
        let temp = tempfile::tempdir().expect("temporary directory");
        let include = temp.path().join("MDCompilers.inc");
        fs::write(&include, "{$DEFINE SAFE}\n").expect("include");

        let directive = Directive {
            kind: DirectiveKind::Include,
            body: "I MDCompilers.Inc".to_string(),
            start: 0,
            end: 0,
        };
        let lookup = resolve_include_path(&directive, &[temp.path().to_path_buf()]);

        assert_eq!(lookup.selected.as_deref(), Some(include.as_path()));
    }

    #[test]
    fn include_lookup_maps_absolute_windows_paths_with_nested_suffixes() {
        let temp = tempfile::tempdir().expect("temporary directory");
        let mapped_root = temp.path().join("sdk");
        let include = mapped_root.join("Nested/Shared.inc");
        fs::create_dir_all(include.parent().expect("nested include directory"))
            .expect("nested include directory");
        fs::write(&include, "{$DEFINE SAFE}\n").expect("mapped include");

        let overrides = EffectiveOverrides {
            path_mappings: vec![PathMapping {
                from: "c:/sdk".to_string(),
                to: mapped_root.clone(),
                config_file: temp.path().join(".delphi-tools.local.toml"),
            }],
            ..EffectiveOverrides::default()
        };
        let directive = Directive {
            kind: DirectiveKind::Include,
            body: "I C:/SDK/Nested/Shared.inc".to_string(),
            start: 0,
            end: 0,
        };
        let lookup = resolve_include_path_with_overrides(
            &directive,
            &[temp.path().to_path_buf()],
            &overrides,
        );

        assert_eq!(lookup.selected.as_deref(), Some(include.as_path()));
        assert_eq!(
            lookup.selected_route,
            IncludeRoute::Mapped {
                root: mapped_root.clone()
            }
        );
        assert!(lookup.error.is_none());
    }

    #[cfg(not(windows))]
    #[test]
    fn include_lookup_snapshots_case_insensitive_directories_and_absence() {
        let temp = tempfile::tempdir().expect("temporary directory");
        let actual_directory = temp.path().join("ActualDir");
        let actual_include = actual_directory.join("Shared.inc");
        let missing_directory = temp.path().join("MissingRoot");
        fs::create_dir_all(&actual_directory).expect("actual directory");
        fs::write(&actual_include, "{$DEFINE SAFE}\n").expect("include");

        let directive = Directive {
            kind: DirectiveKind::Include,
            body: "I actualdir/Shared.Inc".to_string(),
            start: 0,
            end: 0,
        };
        let lookup = resolve_include_path(
            &directive,
            &[missing_directory.clone(), temp.path().to_path_buf()],
        );

        assert_eq!(lookup.selected.as_deref(), Some(actual_include.as_path()));
        assert!(
            lookup
                .observations
                .iter()
                .any(|observation| observation.path == actual_directory
                    && observation.stamp.is_some()),
            "the actual case-insensitive directory must be in the read-set"
        );
        assert!(
            lookup
                .observations
                .iter()
                .any(|observation| observation.path == missing_directory
                    && observation.stamp.is_none()),
            "the absent higher-precedence directory must be in the read-set"
        );
    }

    #[test]
    fn include_byte_budget_is_checked_before_opening_content() {
        let temp = tempfile::tempdir().expect("temporary directory");
        let include = temp.path().join("Large.inc");
        fs::write(&include, "{$DEFINE SAFE}\n").expect("include");

        let read_policy = ReadPolicy::new(
            std::slice::from_ref(&temp.path().to_path_buf()),
            &[],
            &[],
            &EffectiveOverrides::default(),
        );
        let path_entry = ProjectPathEntry {
            path: include.clone(),
            provenance: ProjectPathProvenance::Configured,
        };
        let error = read_include(&include, &read_policy, &path_entry, 1024, Some(1), None)
            .expect_err("remaining include budget is too small");
        assert_eq!(error, super::INCLUDE_BYTE_BUDGET_ERROR);
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
            content_bytes: None,
            candidate_membership: None,
            read_policy: None,
            path_entry: None,
            include_payload: true,
        };

        let error = read_record_content_hash(&include, &record, &AtomicBool::new(false))
            .expect_err("unbound include records must not perform a bare revalidation read");
        assert!(
            error.contains("requester-scoped") || error.contains("authorization"),
            "unexpected missing authorization error: {error}"
        );
    }
}
