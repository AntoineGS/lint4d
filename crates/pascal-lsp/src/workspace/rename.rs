//! Complete, isolated workspace snapshots used by rename and code actions.
//!
//! Navigation intentionally loads a small dependency closure.  Rename cannot
//! use that closure: an unopened reverse consumer may be anywhere in the
//! selected workspace.  This module therefore builds a bounded, throw-away
//! index for each expensive request and never mutates the live navigation
//! cache or the filesystem.

use super::{
    ContextKey, DiskStamp, OpenDocument, PathStamp, Workspace, WorkspaceOptions, absolute_path,
    canonical_file_uri, disk_stamp, is_pascal_path, path_stamp, path_starts_with_ci,
    paths_equal_ci, read_disk_source,
};
use crate::NavigationIndex;
use crate::project::ProjectContext;
use crate::text;
use lsp_types::{
    DocumentChanges, OneOf, OptionalVersionedTextDocumentIdentifier, Position,
    PrepareRenameResponse, TextDocumentEdit, TextEdit, Url, WorkspaceEdit,
};
use pascal_core::decode_bytes;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::hash::{Hash, Hasher};
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use walkdir::WalkDir;

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
const INCLUDE_BYTE_BUDGET_ERROR: &str =
    "include byte limit would be exceeded before reading the file";

#[derive(Debug, Clone)]
pub(crate) struct OverlayInput {
    pub(crate) text: String,
    pub(crate) version: i32,
}

#[derive(Debug, Clone)]
pub(crate) struct WorkspaceInput {
    pub(crate) roots: Vec<PathBuf>,
    pub(crate) options: WorkspaceOptions,
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
}

#[derive(Debug, Clone)]
pub(crate) struct SnapshotSeed {
    pub(crate) record: SourceRecord,
}

impl SnapshotSeed {
    pub(crate) fn new(record: SourceRecord) -> Self {
        Self { record }
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
    pub(crate) editable: HashSet<Url>,
    pub(crate) complete: bool,
    pub(crate) incomplete_reason: Option<String>,
    pub(crate) include_errors: Vec<String>,
    pub(crate) baseline_records: Vec<SourceRecord>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SnapshotMode {
    /// Retain only the requested source and its directly required parse state.
    /// This is sufficient for routine-local bindings, which cannot be
    /// referenced by another unit.
    Local,
    /// Search the configured workspace for reverse references.
    Workspace,
}

#[derive(Debug, Default)]
struct Enumeration {
    paths: Vec<PathBuf>,
    baseline_paths: Vec<BaselinePath>,
    baseline_keys: HashSet<String>,
    baseline_content_hashes: HashMap<String, u64>,
    complete: bool,
    reason: Option<String>,
}

#[derive(Debug, Clone)]
struct BaselinePath {
    path: PathBuf,
    stamp: Option<PathStamp>,
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
            let current =
                read_disk_source(&path, self.options.limits.max_file_bytes).map_err(|error| {
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
        let current =
            read_disk_source(&path, input.options.limits.max_file_bytes).map_err(|error| {
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
    if path_stamp(path) != record.path_stamp {
        return Err(format!(
            "workspace metadata or membership changed while resolving {}; retry the request",
            path.display()
        ));
    }
    if is_cancelled(cancel) {
        return Err(CANCELLATION_MESSAGE.to_string());
    }
    if let Some(expected) = record.content_hash {
        let actual = match file_content_hash(path, cancel) {
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

fn file_content_hash(path: &Path, cancel: &AtomicBool) -> Result<u64, String> {
    if is_cancelled(cancel) {
        return Err(CANCELLATION_MESSAGE.to_string());
    }
    let metadata = fs::metadata(path)
        .map_err(|error| format!("cannot inspect {}: {error}", path.display()))?;
    if !metadata.is_file() {
        return Err(format!("{} is not a regular file", path.display()));
    }
    if metadata.len() > MAX_RENAME_SCAN_FILE_BYTES as u64 {
        return Err(format!(
            "{} exceeds the fixed content hash limit {MAX_RENAME_SCAN_FILE_BYTES}",
            path.display()
        ));
    }
    let mut file =
        fs::File::open(path).map_err(|error| format!("cannot open {}: {error}", path.display()))?;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    let mut buffer = [0_u8; 64 * 1024];
    let mut byte_count = 0usize;
    loop {
        if is_cancelled(cancel) {
            return Err(CANCELLATION_MESSAGE.to_string());
        }
        let read = file
            .read(&mut buffer)
            .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
        if read == 0 {
            break;
        }
        byte_count = byte_count.saturating_add(read);
        if byte_count > MAX_RENAME_SCAN_FILE_BYTES {
            return Err(format!(
                "{} exceeds the fixed content hash limit {MAX_RENAME_SCAN_FILE_BYTES}",
                path.display()
            ));
        }
        hasher.write(&buffer[..read]);
    }
    hasher.write_usize(byte_count);
    if is_cancelled(cancel) {
        return Err(CANCELLATION_MESSAGE.to_string());
    }
    Ok(hasher.finish())
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

fn read_scan_source(path: &Path, cancel: &AtomicBool) -> Result<ScannedSource, String> {
    let metadata = fs::metadata(path)
        .map_err(|error| format!("cannot inspect {}: {error}", path.display()))?;
    if !metadata.is_file() {
        return Err(format!("{} is not a regular file", path.display()));
    }
    if metadata.len() > MAX_RENAME_SCAN_FILE_BYTES as u64 {
        return Err(format!(
            "{} exceeds the fixed rename scan file limit {MAX_RENAME_SCAN_FILE_BYTES}",
            path.display()
        ));
    }
    let mut file =
        fs::File::open(path).map_err(|error| format!("cannot open {}: {error}", path.display()))?;
    let mut bytes = Vec::new();
    let mut content_hasher = std::collections::hash_map::DefaultHasher::new();
    let mut buffer = [0_u8; 64 * 1024];
    let mut byte_count = 0usize;
    loop {
        if is_cancelled(cancel) {
            return Err(CANCELLATION_MESSAGE.to_string());
        }
        let read = file
            .read(&mut buffer)
            .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
        if read == 0 {
            break;
        }
        byte_count = byte_count.saturating_add(read);
        if byte_count > MAX_RENAME_SCAN_FILE_BYTES {
            return Err(format!(
                "{} exceeds the fixed rename scan file limit {MAX_RENAME_SCAN_FILE_BYTES}",
                path.display()
            ));
        }
        content_hasher.write(&buffer[..read]);
        bytes.extend_from_slice(&buffer[..read]);
    }
    content_hasher.write_usize(byte_count);
    if is_cancelled(cancel) {
        return Err(CANCELLATION_MESSAGE.to_string());
    }
    if let Some(encoding) = unsupported_source_encoding(&bytes) {
        return Err(format!(
            "{} uses unsupported {encoding} source encoding",
            path.display()
        ));
    }
    Ok(ScannedSource {
        bytes: byte_count,
        data: bytes,
        content_hash: content_hasher.finish(),
    })
}

fn unsupported_source_encoding(bytes: &[u8]) -> Option<&'static str> {
    match bytes {
        [0xFF, 0xFE, ..] => Some("UTF-16LE"),
        [0xFE, 0xFF, ..] => Some("UTF-16BE"),
        _ => None,
    }
}

fn path_record_at(
    path: PathBuf,
    stamp: Option<PathStamp>,
    content_hash: Option<u64>,
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
    })
}

pub(crate) fn snapshot_records(snapshot: &RenameSnapshot) -> Vec<SourceRecord> {
    let mut records = snapshot.records.values().cloned().collect::<Vec<_>>();
    records.extend(snapshot.baseline_records.iter().cloned());
    records
}

pub(crate) fn source_for_input(
    input: &WorkspaceInput,
    uri: &Url,
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
            },
        ));
    }

    let path = uri
        .to_file_path()
        .map(absolute_path)
        .map_err(|_| format!("not a file URI: {uri}"))?;
    let disk = read_disk_source(&path, input.options.limits.max_file_bytes)
        .map_err(|error| format!("could not read source {uri}: {error}"))?;
    let record = SourceRecord {
        uri: uri.clone(),
        text: disk.text.clone(),
        version: None,
        stamp: Some(disk.stamp),
        open: false,
        path: None,
        path_stamp: None,
        content_hash: Some(disk.content_hash),
    };
    Ok((disk.text, record))
}

pub(crate) fn input_source_is_editable(input: &WorkspaceInput, uri: &Url) -> bool {
    let workspace = Workspace::new(input.roots.clone(), input.options.clone());
    let Ok(path) = uri.to_file_path() else {
        return false;
    };
    let path = absolute_path(path);
    workspace.accepts_path(&path) && is_editable_source_path(&workspace, &path)
}

type InputBindingInfo = (
    String,
    SourceRecord,
    Option<(crate::navigation::RenameBindingInfo, bool)>,
);

fn binding_info_for_input(
    input: &WorkspaceInput,
    uri: &Url,
    position: Position,
    additional_names: &[String],
) -> Result<InputBindingInfo, String> {
    let (source, record) = source_for_input(input, uri)?;
    let info = binding_info_for_source(uri, &source, position, additional_names).ok();
    Ok((source, record, info))
}

fn binding_info_for_source(
    uri: &Url,
    source: &str,
    position: Position,
    additional_names: &[String],
) -> Result<(crate::navigation::RenameBindingInfo, bool), String> {
    let uri = canonical_file_uri(uri);
    let mut index = NavigationIndex::new();
    index
        .update(uri.clone(), source.to_owned())
        .map_err(|error| format!("could not index rename source {uri}: {error}"))?;
    let info = index.rename_binding_info(&uri, position)?;
    let self_contained = index.self_contained_rename_binding(&uri, position, additional_names);
    Ok((info, self_contained))
}

fn contains_identifier(source: &str, name: &str) -> bool {
    let name = name.as_bytes();
    if name.is_empty() {
        return false;
    }
    let source = source.as_bytes();
    source
        .windows(name.len())
        .enumerate()
        .any(|(index, candidate)| {
            candidate.eq_ignore_ascii_case(name)
                && (index == 0 || !is_identifier_byte(source[index - 1]))
                && (index + name.len() == source.len()
                    || !is_identifier_byte(source[index + name.len()]))
        })
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
    let (initial_source, _) = match source_for_input(&input, &uri) {
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
    let (planning_source, target_record, binding_info) =
        match binding_info_for_input(&input, &uri, position, &[]) {
            Ok(result) => result,
            Err(error) => return failed(source_generation, configuration_generation, error),
        };
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
        Some(SnapshotSeed::new(target_record)),
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

    if let Err(error) = check_includes(&snapshot, &uri, position) {
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
    let (initial_source, _) = match source_for_input(&input, &uri) {
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
    let (planning_source, target_record, binding_info) =
        match binding_info_for_input(&input, &uri, position, &additional_names) {
            Ok(result) => result,
            Err(error) => return failed(source_generation, configuration_generation, error),
        };
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
        Some(SnapshotSeed::new(target_record)),
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
    if let Err(error) = check_includes(&snapshot, &uri, position) {
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
    loader_options.limits.max_files = loader_options
        .limits
        .max_files
        .saturating_add(MAX_SNAPSHOT_DEPENDENCY_FILES);
    loader_options.limits.max_total_bytes = loader_options.limits.max_total_bytes.saturating_add(
        loader_options
            .limits
            .max_file_bytes
            .saturating_mul(MAX_SNAPSHOT_DEPENDENCY_FILES),
    );
    let mut loader = Workspace::new(input.roots.clone(), loader_options);
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

    let priority = priority.iter().map(canonical_file_uri).collect::<Vec<_>>();
    if mode == SnapshotMode::Workspace {
        for uri in &priority {
            if is_cancelled(cancel) {
                return Err(CANCELLATION_MESSAGE.to_string());
            }
            let Some(context_key) = loader.context_for_uri(uri) else {
                return Err(format!(
                    "rename workspace scan incomplete: project context could not be resolved for {uri}"
                ));
            };
            if !loader
                .contexts
                .get(&context_key)
                .is_some_and(|state| state.context.discovery_complete)
            {
                return Err(format!(
                    "rename workspace scan incomplete: project context is ambiguous or incomplete for {uri}"
                ));
            }
        }
    }
    let enumeration = enumerate_sources(&loader, input, &priority, mode, cancel)?;
    let mut paths = enumeration.paths;
    let mut complete = enumeration.complete;
    let mut incomplete_reason = enumeration.reason;
    let mut baseline_paths = enumeration.baseline_paths;
    let mut baseline_keys = enumeration.baseline_keys;
    let mut baseline_content_hashes = enumeration.baseline_content_hashes;
    for rejected_uri in &input.rejected_documents {
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
    paths.sort_by(|left, right| left.to_string_lossy().cmp(&right.to_string_lossy()));
    paths.sort_by_key(|path| {
        priority
            .iter()
            .position(|uri| {
                uri.to_file_path()
                    .ok()
                    .is_some_and(|priority_path| paths_equal_ci(&priority_path, path))
            })
            .unwrap_or(priority.len())
    });

    let mut sources = HashMap::new();
    let mut records = HashMap::new();
    let mut editable = HashSet::new();
    let mut contexts = HashMap::new();
    let mut index = NavigationIndex::new();
    let mut indexed_sizes = HashMap::new();
    let mut indexed_stamps = HashMap::new();
    let mut indexed_uris = HashSet::new();
    let mut retained_files = 0usize;
    let mut retained_bytes = 0usize;
    let mut scanned_bytes = 0usize;
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
        match loader.context_for_uri(uri) {
            Some(context_key) => {
                capture_context_baseline(
                    &loader,
                    &context_key,
                    &mut baseline_paths,
                    &mut baseline_keys,
                    &mut baseline_content_hashes,
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
            None => {
                complete = false;
                incomplete_reason.get_or_insert_with(|| {
                    format!("project context could not be resolved for {uri}")
                });
            }
        }
    }
    for path in paths {
        if is_cancelled(cancel) {
            return Err(CANCELLATION_MESSAGE.to_string());
        }
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
                },
                overlay.text.len(),
            )
        } else {
            let scan = match read_scan_source(&path, cancel) {
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
                },
                scan.bytes,
            )
        };
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

                let mut summary_record = record;
                summary_record.text = summary.clone();
                if summary_record.open {
                    summary_record.content_hash = Some(text_content_hash(&source));
                } else {
                    let stamp = summary_record.stamp.take();
                    summary_record.path = Some(path.clone());
                    summary_record.path_stamp = stamp.map(|stamp| PathStamp {
                        bytes: stamp.bytes,
                        modified: stamp.modified,
                        is_dir: false,
                    });
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

        if mode == SnapshotMode::Workspace && !contexts.contains_key(&uri) {
            let context = loader.context_for_uri(&uri);
            if let Some(context_key) = context {
                capture_context_baseline(
                    &loader,
                    &context_key,
                    &mut baseline_paths,
                    &mut baseline_keys,
                    &mut baseline_content_hashes,
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
            } else {
                complete = false;
                incomplete_reason.get_or_insert_with(|| {
                    format!("project context could not be resolved for {uri}")
                });
            }
        }
        index
            .update(uri.clone(), source.clone())
            .map_err(|error| format!("rename workspace scan could not index {uri}: {error}"))?;
        retained_files = retained_files.saturating_add(1);
        retained_bytes = retained_bytes.saturating_add(source_bytes);
        indexed_uris.insert(uri.clone());
        indexed_sizes.insert(uri.clone(), source.len());
        if let Some(stamp) = record.stamp.clone() {
            indexed_stamps.insert(uri.clone(), stamp);
        }
        sources.insert(uri.clone(), source);
        if is_editable_source_path(&loader, &path) {
            editable.insert(uri.clone());
        }
        records.insert(uri, record);
    }

    if sources.is_empty() {
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
            let dependencies = loader.load_imports(&uri, &context_key, &mut pins);
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

    if let Some(evicted_uri) = pins.iter().find(|uri| !loader.indexed_files.contains(*uri)) {
        complete = false;
        incomplete_reason.get_or_insert_with(|| {
            format!("retained rename source was evicted before binding completed: {evicted_uri}")
        });
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
        let (source, record) = if let Some(overlay) = input.overlays.get(&uri) {
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
            add_baseline_path_with_stamp(
                &mut baseline_paths,
                &mut baseline_keys,
                path.clone(),
                Some(PathStamp {
                    bytes: stamp.bytes,
                    modified: stamp.modified,
                    is_dir: false,
                }),
            );
            add_baseline_content_hash(&path, &mut baseline_content_hashes, cancel, true)?;
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
                },
            )
        };
        if let Some(context_key) = loader.document_contexts.get(&uri).cloned() {
            contexts.insert(uri.clone(), context_key);
        }
        if is_editable_source_path(&loader, &path) {
            editable.insert(uri.clone());
        }
        sources.insert(uri.clone(), source);
        records.insert(uri, record);
    }

    let include_audit = audit_includes(IncludeAuditor {
        sources: &sources,
        loader: &mut loader,
        contexts: &mut contexts,
        baseline_paths: &mut baseline_paths,
        baseline_keys: &mut baseline_keys,
        baseline_content_hashes: &mut baseline_content_hashes,
        candidate_names,
        max_file_bytes: input.options.limits.max_file_bytes,
        cancel,
        cache: HashMap::new(),
        active: HashSet::new(),
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

    for context_key in loader.contexts.keys().cloned().collect::<Vec<_>>() {
        capture_context_baseline(
            &loader,
            &context_key,
            &mut baseline_paths,
            &mut baseline_keys,
            &mut baseline_content_hashes,
            cancel,
        )?;
    }
    let baseline_records = baseline_paths
        .into_iter()
        .filter_map(|baseline| {
            let content_hash = baseline_content_hashes
                .get(&path_key(&baseline.path))
                .copied();
            path_record_at(baseline.path, baseline.stamp, content_hash)
        })
        .collect::<Vec<_>>();

    Ok(RenameSnapshot {
        index: loader.index,
        sources,
        records,
        editable,
        complete,
        incomplete_reason,
        include_errors,
        baseline_records,
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
    for root in &workspace.roots {
        let config_path = root.excludes.config_root.join(".lint4d.toml");
        add_baseline_path(
            &mut result.baseline_paths,
            &mut result.baseline_keys,
            config_path.clone(),
        );
        if let Err(error) = add_baseline_content_hash(
            &config_path,
            &mut result.baseline_content_hashes,
            cancel,
            config_path.is_file(),
        ) {
            if error == CANCELLATION_MESSAGE {
                return Err(error);
            }
            result.complete = false;
            result.reason.get_or_insert_with(|| {
                format!(
                    "could not fingerprint configuration {}: {error}",
                    config_path.display()
                )
            });
        }
    }
    let mut paths: Vec<PathBuf> = Vec::new();
    let mut path_keys = HashSet::new();
    if mode == SnapshotMode::Local {
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
                add_baseline_path(
                    &mut result.baseline_paths,
                    &mut result.baseline_keys,
                    path.clone(),
                );
                if path_keys.insert(path_key(&path)) {
                    paths.push(path);
                }
            }
        }
        result.paths = paths;
        return Ok(result);
    }
    let mut visited_entries = 0usize;
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
                visited_entries = visited_entries.saturating_add(1);
                if visited_entries > entry_limit {
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
                    add_baseline_path(&mut result.baseline_paths, &mut result.baseline_keys, path);
                    continue;
                }
                if file_type.is_file() && root.accepts(&path) && is_project_metadata_path(&path) {
                    add_baseline_path(
                        &mut result.baseline_paths,
                        &mut result.baseline_keys,
                        path.clone(),
                    );
                    if let Err(error) = add_baseline_content_hash(
                        &path,
                        &mut result.baseline_content_hashes,
                        cancel,
                        true,
                    ) {
                        if error == CANCELLATION_MESSAGE {
                            return Err(error);
                        }
                        result.complete = false;
                        result.reason.get_or_insert_with(|| {
                            format!(
                                "could not fingerprint project metadata {}: {error}",
                                path.display()
                            )
                        });
                    }
                }
                if !file_type.is_file() || !is_pascal_path(&path) || !root.accepts(&path) {
                    continue;
                }
                if !path_keys.insert(path_key(&path)) {
                    continue;
                }
                add_baseline_path(
                    &mut result.baseline_paths,
                    &mut result.baseline_keys,
                    path.clone(),
                );
                paths.push(path);
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
        if (path.is_file() || input.overlays.contains_key(uri)) && path_keys.insert(path_key(&path))
        {
            add_baseline_path(
                &mut result.baseline_paths,
                &mut result.baseline_keys,
                path.clone(),
            );
            paths.push(path);
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
            } else if path_keys.insert(path_key(&path)) {
                if overlay.text.len() <= input.options.limits.max_file_bytes {
                    add_baseline_path(
                        &mut result.baseline_paths,
                        &mut result.baseline_keys,
                        path.clone(),
                    );
                    paths.push(path);
                } else {
                    result.complete = false;
                    result.reason.get_or_insert_with(|| {
                        format!("open overlay {uri} exceeds the configured per-file limit")
                    });
                }
            }
        }
    }

    paths.sort_by(|left, right| left.to_string_lossy().cmp(&right.to_string_lossy()));
    paths.dedup_by(|left, right| path_key(left) == path_key(right));
    let priority_paths: Vec<PathBuf> = priority
        .iter()
        .filter_map(|uri| uri.to_file_path().ok().map(absolute_path))
        .collect();
    paths.sort_by_key(|path| {
        priority_paths
            .iter()
            .position(|priority| paths_equal_ci(priority, path))
            .unwrap_or(priority_paths.len())
    });

    result.paths = paths;
    Ok(result)
}

fn add_baseline_path(
    baseline_paths: &mut Vec<BaselinePath>,
    baseline_keys: &mut HashSet<String>,
    path: PathBuf,
) {
    let stamp = path_stamp(&path);
    add_baseline_path_with_stamp(baseline_paths, baseline_keys, path, stamp);
}

fn add_baseline_path_with_stamp(
    baseline_paths: &mut Vec<BaselinePath>,
    baseline_keys: &mut HashSet<String>,
    path: PathBuf,
    stamp: Option<PathStamp>,
) {
    if !baseline_keys.insert(path_key(&path)) {
        return;
    }
    baseline_paths.push(BaselinePath { path, stamp });
}

fn add_baseline_content_hash(
    path: &Path,
    content_hashes: &mut HashMap<String, u64>,
    cancel: &AtomicBool,
    require_file: bool,
) -> Result<(), String> {
    if is_cancelled(cancel) {
        return Err(CANCELLATION_MESSAGE.to_string());
    }
    let key = path_key(path);
    if content_hashes.contains_key(&key) {
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
    let content_hash = file_content_hash(path, cancel)?;
    content_hashes.insert(key, content_hash);
    Ok(())
}

fn capture_context_baseline(
    workspace: &Workspace,
    context_key: &super::ContextKey,
    baseline_paths: &mut Vec<BaselinePath>,
    baseline_keys: &mut HashSet<String>,
    baseline_content_hashes: &mut HashMap<String, u64>,
    cancel: &AtomicBool,
) -> Result<(), String> {
    if let Some(state) = workspace.contexts.get(context_key) {
        for (path, stamp) in &state.watched_paths {
            if is_cancelled(cancel) {
                return Err(CANCELLATION_MESSAGE.to_string());
            }
            add_baseline_path_with_stamp(
                baseline_paths,
                baseline_keys,
                path.clone(),
                stamp.clone(),
            );
            if stamp.as_ref().is_some_and(|stamp| !stamp.is_dir) {
                add_baseline_content_hash(path, baseline_content_hashes, cancel, true)?;
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
) -> Result<(), String> {
    if let Some(error) = snapshot.include_errors.first() {
        return Err(error.clone());
    }
    let target_name = snapshot
        .sources
        .get(uri)
        .and_then(|source| identifier_at_position(source, position));
    for (source_uri, source) in &snapshot.sources {
        let relevant = source_uri == uri
            || target_name
                .as_deref()
                .is_some_and(|name| contains_identifier(source, name));
        if relevant {
            let regions = conditional_regions(source);
            if regions.iter().any(|(start, end)| {
                (source_uri == uri
                    && text::position_to_offset(source, position)
                        .is_some_and(|offset| (*start..=*end).contains(&offset)))
                    || target_name
                        .as_deref()
                        .is_some_and(|name| contains_identifier(&source[*start..*end], name))
            }) {
                return Err(format!(
                    "rename cannot prove completeness because relevant conditional compilation affects {source_uri}"
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

#[derive(Debug)]
struct IncludeLookup {
    observations: Vec<IncludeObservation>,
    selected: Option<PathBuf>,
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
    baseline_paths: &'a mut Vec<BaselinePath>,
    baseline_keys: &'a mut HashSet<String>,
    baseline_content_hashes: &'a mut HashMap<String, u64>,
    candidate_names: &'a [String],
    max_file_bytes: usize,
    cancel: &'a AtomicBool,
    cache: HashMap<String, IncludeAnalysis>,
    active: HashSet<String>,
    files_read: usize,
    bytes_read: usize,
    directives_seen: usize,
    stopped: bool,
    result: IncludeAuditResult,
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
        let Some(source) = auditor.sources.get(&uri) else {
            continue;
        };
        let source_directives = directives(source);
        if source_directives
            .iter()
            .any(|directive| directive.kind == DirectiveKind::Other)
        {
            auditor.record_error(format!(
                "rename cannot prove completeness because include owner {uri} contains an unsupported directive"
            ));
            auditor.stopped = true;
            break;
        }
        let include_directives = source_directives
            .into_iter()
            .filter(|directive| directive.kind == DirectiveKind::Include)
            .collect::<Vec<_>>();
        if include_directives.is_empty() {
            continue;
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
            auditor.inspect_top_level(&uri, &directive, context.as_ref())?;
        }
    }

    Ok(auditor.result)
}

impl IncludeAuditor<'_> {
    fn context_for_source(&mut self, uri: &Url) -> Result<Option<ProjectContext>, String> {
        let context_key = if let Some(context_key) = self.contexts.get(uri).cloned() {
            Some(context_key)
        } else if let Some(context_key) = self.loader.document_contexts.get(uri).cloned() {
            self.contexts.insert(uri.clone(), context_key.clone());
            Some(context_key)
        } else {
            let context_key = self.loader.context_for_uri(uri);
            if let Some(context_key) = &context_key {
                self.contexts.insert(uri.clone(), context_key.clone());
            }
            context_key
        };

        let Some(context_key) = context_key else {
            return Ok(None);
        };
        let workspace: &Workspace = &*self.loader;
        capture_context_baseline(
            workspace,
            &context_key,
            self.baseline_paths,
            self.baseline_keys,
            self.baseline_content_hashes,
            self.cancel,
        )?;
        let Some(state) = self.loader.contexts.get(&context_key) else {
            return Ok(None);
        };
        let context = state.context.clone();
        if !context.discovery_complete {
            self.result.incomplete_reason.get_or_insert_with(|| {
                format!("project context is ambiguous or incomplete for {uri}")
            });
        }
        Ok(Some(context))
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
        context: Option<&ProjectContext>,
    ) -> Result<(), String> {
        let Some(owner_path) = uri.to_file_path().ok().map(absolute_path) else {
            self.record_error(format!(
                "rename cannot prove completeness because an include path in {uri} is unresolved"
            ));
            self.stopped = true;
            return Ok(());
        };

        let directories = include_search_directories(&owner_path, context);
        let lookup = resolve_include_path(directive, &directories);
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

        let analysis = self.inspect_include_file(&path, context, 0)?;
        if !analysis.safe || analysis.relevant {
            let reason = analysis
                .reason
                .unwrap_or_else(|| format!("include {path:?} contains source content"));
            self.record_error(format!("rename cannot prove completeness because {reason}"));
            self.stopped = true;
        }
        Ok(())
    }

    fn inspect_nested(
        &mut self,
        owner_path: &Path,
        directive: &Directive,
        context: Option<&ProjectContext>,
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
        let directories = include_search_directories(owner_path, context);
        let lookup = resolve_include_path(directive, &directories);
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
        self.inspect_include_file(&path, context, depth + 1)
    }

    fn inspect_include_file(
        &mut self,
        path: &Path,
        context: Option<&ProjectContext>,
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
        let directories = include_search_directories(path, context);
        let cache_key = include_cache_key(path, &directories);
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
        let include_source = match read_include(
            path,
            self.max_file_bytes,
            Some(remaining_bytes),
            Some(self.cancel),
        ) {
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

        let directive_only = directive_only_directives(&include_source.text, true);
        let relevant = directive_only.is_none()
            && contains_any_identifier(&include_source.text, self.candidate_names);
        let include_directives = directive_only.unwrap_or_else(|| directives(&include_source.text));
        let analysis = if self.bytes_read >= MAX_RENAME_INCLUDE_BYTES {
            self.stopped = true;
            IncludeAnalysis::unsafe_with_reason(
                format!(
                    "include {path:?} could not be audited because include byte limit ({MAX_RENAME_INCLUDE_BYTES}) was reached"
                ),
                relevant,
            )
        } else if include_directives
            .iter()
            .any(|directive| directive.kind == DirectiveKind::Other)
        {
            IncludeAnalysis::unsafe_with_reason(
                format!("include {path:?} contains an unsupported directive"),
                relevant,
            )
        } else if conditional_include_references_candidate(
            &include_directives,
            self.candidate_names,
        ) {
            IncludeAnalysis::unsafe_with_reason(
                format!("include {path:?} has a Pascal-dependent conditional expression"),
                true,
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
                let child = self.inspect_nested(path, &directive, context, depth)?;
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
                self.baseline_paths,
                self.baseline_keys,
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

fn include_cache_key(path: &Path, directories: &[PathBuf]) -> String {
    let mut key = path_key(path);
    key.push('\0');
    for directory in directories {
        key.push_str(&path_key(directory));
        key.push('\0');
    }
    key
}

fn canonical_include_key(path: &Path) -> String {
    fs::canonicalize(path)
        .map(|canonical| path_key(&canonical))
        .unwrap_or_else(|_| path_key(path))
}

fn resolve_include_path(directive: &Directive, directories: &[PathBuf]) -> IncludeLookup {
    let Some(raw) = include_name(directive) else {
        return IncludeLookup {
            observations: Vec::new(),
            selected: None,
            error: None,
        };
    };

    let mut observations = Vec::new();
    let mut selected = None;
    let mut error = None;
    for directory in directories {
        observations.push(IncludeObservation {
            path: directory.clone(),
            stamp: fs::metadata(directory)
                .ok()
                .map(|metadata| path_stamp_from_metadata(&metadata)),
        });
        let candidate = absolute_path(directory.join(&raw));
        if observations
            .iter()
            .any(|observation| path_key(&observation.path) == path_key(&candidate))
        {
            continue;
        }
        let metadata = fs::metadata(&candidate);
        let stamp = metadata.as_ref().ok().map(path_stamp_from_metadata);
        observations.push(IncludeObservation {
            path: candidate.clone(),
            stamp,
        });
        match metadata {
            Ok(metadata) if metadata.is_file() => {
                selected = Some(candidate);
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
                        stamp: actual_metadata.as_ref().ok().map(path_stamp_from_metadata),
                    });
                    match actual_metadata {
                        Ok(metadata) if metadata.is_file() => {
                            selected = Some(actual);
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
        stamp: fs::metadata(&current)
            .ok()
            .map(|metadata| path_stamp_from_metadata(&metadata)),
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
                    Ok(metadata) => Some(path_stamp_from_metadata(&metadata)),
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

fn path_stamp_from_metadata(metadata: &fs::Metadata) -> PathStamp {
    PathStamp {
        bytes: metadata.len(),
        modified: metadata.modified().ok(),
        is_dir: metadata.is_dir(),
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

fn conditional_include_references_candidate(
    directives: &[Directive],
    candidate_names: &[String],
) -> bool {
    if candidate_names
        .iter()
        .any(|name| !name.trim_start_matches('&').is_ascii())
    {
        return true;
    }
    directives.iter().any(|directive| {
        let keyword = directive_keyword(&directive.body)
            .map(str::to_ascii_lowercase)
            .unwrap_or_default();
        if keyword != "if" && keyword != "elseif" && keyword != "elif" {
            return false;
        }
        let expression = directive_arguments(&directive.body);
        identifier_spans(expression)
            .into_iter()
            .any(|(start, end)| {
                let identifier = &expression[start..end];
                candidate_names.iter().any(|name| {
                    identifier.eq_ignore_ascii_case(name.trim_start_matches('&'))
                        && !is_defined_compiler_symbol(expression, start)
                })
            })
    })
}

fn is_defined_compiler_symbol(expression: &str, identifier_start: usize) -> bool {
    let prefix = expression[..identifier_start].trim_end();
    let Some(prefix) = prefix.strip_suffix('(') else {
        return false;
    };
    let prefix = prefix.trim_end();
    let start = prefix
        .as_bytes()
        .iter()
        .rposition(|byte| !is_identifier_byte(*byte))
        .map_or(0, |index| index + 1);
    if !prefix[start..].eq_ignore_ascii_case("defined") {
        return false;
    }
    !prefix[..start].trim_end().ends_with('.')
}

fn directive_keyword(body: &str) -> Option<&str> {
    body.trim_start()
        .split(|character: char| character.is_ascii_whitespace() || character == ':')
        .next()
        .filter(|keyword| !keyword.is_empty())
}

fn directive_arguments(body: &str) -> &str {
    let body = body.trim_start();
    let Some(keyword) = directive_keyword(body) else {
        return "";
    };
    body.get(keyword.len()..)
        .map(str::trim_start)
        .unwrap_or_default()
}

fn identifier_spans(source: &str) -> Vec<(usize, usize)> {
    let bytes = source.as_bytes();
    let mut spans = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'\'' {
            skip_string(bytes, &mut index);
            continue;
        }
        if is_identifier_byte(bytes[index]) {
            let start = index;
            index += 1;
            while index < bytes.len() && is_identifier_byte(bytes[index]) {
                index += 1;
            }
            spans.push((start, index));
        } else {
            index += 1;
        }
    }
    spans
}

#[derive(Debug, Clone)]
struct IncludeSource {
    text: String,
    content_hash: u64,
    bytes: usize,
}

fn read_include(
    path: &Path,
    max_file_bytes: usize,
    max_total_bytes: Option<usize>,
    cancel: Option<&AtomicBool>,
) -> Result<IncludeSource, String> {
    if cancel.is_some_and(is_cancelled) {
        return Err(CANCELLATION_MESSAGE.to_string());
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
    let mut file = fs::File::open(path).map_err(|error| error.to_string())?;
    let mut bytes = Vec::with_capacity(metadata.len().min(max_file_bytes as u64) as usize);
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    let mut buffer = [0_u8; 64 * 1024];
    let mut byte_count = 0usize;
    loop {
        if cancel.is_some_and(is_cancelled) {
            return Err(CANCELLATION_MESSAGE.to_string());
        }
        let read_limit = max_total_bytes
            .map(|limit| limit.saturating_sub(byte_count))
            .unwrap_or(buffer.len())
            .min(buffer.len());
        if read_limit == 0 {
            break;
        }
        let read = file
            .read(&mut buffer[..read_limit])
            .map_err(|error| error.to_string())?;
        if read == 0 {
            break;
        }
        byte_count = byte_count.saturating_add(read);
        if byte_count > max_file_bytes {
            return Err(format!(
                "file exceeds the configured per-file limit {max_file_bytes}"
            ));
        }
        hasher.write(&buffer[..read]);
        bytes.extend_from_slice(&buffer[..read]);
    }
    hasher.write_usize(byte_count);
    if cancel.is_some_and(is_cancelled) {
        return Err(CANCELLATION_MESSAGE.to_string());
    }
    if let Some(encoding) = unsupported_source_encoding(&bytes) {
        return Err(format!("unsupported {encoding} source encoding"));
    }
    Ok(IncludeSource {
        text: decode_bytes(&bytes).into_owned(),
        content_hash: hasher.finish(),
        bytes: byte_count,
    })
}

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
    use super::{
        Directive, DirectiveKind, contains_any_identifier, directive_kind, read_include,
        resolve_include_path,
    };
    use std::fs;

    #[test]
    fn contains_any_identifier_uses_identifier_boundaries() {
        let names = ["Foo".to_string(), "&Bar".to_string()];

        assert!(contains_any_identifier("value := FOO; &bar := 1;", &names));
        assert!(!contains_any_identifier("value := Foobar;", &names));
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

        let error = read_include(&include, 1024, Some(1), None)
            .expect_err("remaining include budget is too small");
        assert_eq!(error, super::INCLUDE_BYTE_BUDGET_ERROR);
    }
}
