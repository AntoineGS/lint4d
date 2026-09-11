//! Complete, isolated workspace snapshots used by rename and code actions.
//!
//! Navigation intentionally loads a small dependency closure.  Rename cannot
//! use that closure: an unopened reverse consumer may be anywhere in the
//! selected workspace.  This module therefore builds a bounded, throw-away
//! index for each expensive request and never mutates the live navigation
//! cache or the filesystem.

use super::{
    DiskStamp, OpenDocument, PathStamp, Workspace, WorkspaceOptions, absolute_path,
    canonical_file_uri, disk_stamp, is_pascal_path, path_stamp, path_starts_with_ci,
    paths_equal_ci, read_disk_source,
};
use crate::NavigationIndex;
use crate::text;
use lsp_types::{
    DocumentChanges, OneOf, OptionalVersionedTextDocumentIdentifier, Position,
    PrepareRenameResponse, TextDocumentEdit, TextEdit, Url, WorkspaceEdit,
};
use pascal_core::decode_bytes;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::hash::Hasher;
use std::io::Read;
use std::path::{Path, PathBuf};
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
    pub(crate) max_file_bytes: usize,
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
                if document.version != record.version.unwrap_or_default()
                    || document.text.as_deref() != Some(record.text.as_str())
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
            if overlay.version != record.version.unwrap_or_default() || overlay.text != record.text
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

fn binding_info_for_input(
    input: &WorkspaceInput,
    uri: &Url,
    position: Position,
) -> Result<
    (
        String,
        SourceRecord,
        Option<crate::navigation::RenameBindingInfo>,
    ),
    String,
> {
    let (source, record) = source_for_input(input, uri)?;
    let info = binding_info_for_source(uri, &source, position).ok();
    Ok((source, record, info))
}

fn binding_info_for_source(
    uri: &Url,
    source: &str,
    position: Position,
) -> Result<crate::navigation::RenameBindingInfo, String> {
    let uri = canonical_file_uri(uri);
    let mut index = NavigationIndex::new();
    index
        .update(uri.clone(), source.to_owned())
        .map_err(|error| format!("could not index rename source {uri}: {error}"))?;
    index.rename_binding_info(&uri, position)
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
        match binding_info_for_input(&input, &uri, position) {
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
    let (mode, candidate_names) = match binding_info {
        Some(info) => {
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
            )
        }
        None => (SnapshotMode::Workspace, vec![original_name]),
    };
    let snapshot = match build_snapshot(
        &input,
        std::slice::from_ref(&uri),
        &candidate_names,
        mode,
        Some(SnapshotSeed::new(target_record)),
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
    let (planning_source, target_record, binding_info) =
        match binding_info_for_input(&input, &uri, position) {
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
    let (mode, candidate_names) = match binding_info {
        Some(info) => {
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
            )
        }
        None => (
            SnapshotMode::Workspace,
            vec![original_name, new_name.to_string()],
        ),
    };
    let snapshot = match build_snapshot(
        &input,
        std::slice::from_ref(&uri),
        &candidate_names,
        mode,
        Some(SnapshotSeed::new(target_record)),
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
        for include_path in include_paths(&uri, &source) {
            add_baseline_path(&mut baseline_paths, &mut baseline_keys, include_path);
        }
        if mode == SnapshotMode::Workspace {
            if let Some(error) = relevant_include_error(
                &uri,
                &source,
                candidate_names,
                &mut baseline_content_hashes,
                cancel,
            )? {
                include_errors.push(error);
            }
        }
        if !should_index {
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
        for include_path in include_paths(&uri, &source) {
            add_baseline_path(&mut baseline_paths, &mut baseline_keys, include_path);
        }
        if let Some(context_key) = loader.document_contexts.get(&uri).cloned() {
            contexts.insert(uri.clone(), context_key);
        }
        if is_editable_source_path(&loader, &path) {
            editable.insert(uri.clone());
        }
        sources.insert(uri.clone(), source);
        records.insert(uri, record);
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
        max_file_bytes: input.options.limits.max_file_bytes,
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

        for directive in directives(source)
            .into_iter()
            .filter(|directive| directive.kind == DirectiveKind::Include)
        {
            let Some(path) = include_path(source_uri, source, &directive) else {
                return Err(format!(
                    "rename cannot prove completeness because a relevant include path in {source_uri} is unresolved"
                ));
            };
            let include_source = match read_include(&path, snapshot.max_file_bytes, None) {
                Ok(source) => source,
                Err(error) => {
                    return Err(format!(
                        "rename cannot prove completeness because include {path:?} could not be read: {error}"
                    ));
                }
            };
            let include_relevant = target_name
                .as_deref()
                .is_some_and(|name| contains_identifier(&include_source.text, name));
            if !is_harmless_include(&include_source.text) || include_relevant {
                return Err(format!(
                    "rename cannot prove completeness because include {path:?} contains source content"
                ));
            }
        }
    }
    Ok(())
}

fn relevant_include_error(
    uri: &Url,
    source: &str,
    candidate_names: &[String],
    baseline_content_hashes: &mut HashMap<String, u64>,
    cancel: &AtomicBool,
) -> Result<Option<String>, String> {
    for directive in directives(source)
        .into_iter()
        .filter(|directive| directive.kind == DirectiveKind::Include)
    {
        let Some(path) = include_path(uri, source, &directive) else {
            return Ok(Some(format!(
                "rename cannot prove completeness because an include path in {uri} is unresolved"
            )));
        };
        let include_source = match read_include(&path, MAX_RENAME_SCAN_FILE_BYTES, Some(cancel)) {
            Ok(source) => source,
            Err(error) if error == CANCELLATION_MESSAGE => return Err(error),
            Err(error) => {
                return Ok(Some(format!(
                    "rename cannot prove completeness because include {path:?} could not be read: {error}"
                )));
            }
        };
        baseline_content_hashes
            .entry(path_key(&path))
            .or_insert(include_source.content_hash);
        let include_relevant = contains_any_identifier(&include_source.text, candidate_names);
        if include_relevant || !is_harmless_include(&include_source.text) {
            return Ok(Some(format!(
                "rename cannot prove completeness because include {path:?} contains source content"
            )));
        }
    }
    Ok(None)
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

fn include_paths(uri: &Url, source: &str) -> Vec<PathBuf> {
    directives(source)
        .into_iter()
        .filter(|directive| directive.kind == DirectiveKind::Include)
        .filter_map(|directive| include_path(uri, source, &directive))
        .collect()
}

fn include_path(uri: &Url, _source: &str, directive: &Directive) -> Option<PathBuf> {
    let raw = directive
        .body
        .trim_start()
        .split_once(|character: char| character.is_ascii_whitespace() || character == ':')
        .map(|(_, remainder)| remainder.trim())
        .filter(|remainder| !remainder.is_empty())?
        .trim_matches(|character| character == '\'' || character == '"');
    if raw.is_empty() || raw.contains("$(") {
        return None;
    }
    let path = uri.to_file_path().ok()?;
    let parent = absolute_path(path).parent()?.to_path_buf();
    Some(absolute_path(parent.join(raw.replace('\\', "/"))))
}

struct IncludeSource {
    text: String,
    content_hash: u64,
}

fn read_include(
    path: &Path,
    max_file_bytes: usize,
    cancel: Option<&AtomicBool>,
) -> Result<IncludeSource, String> {
    let metadata = fs::metadata(path).map_err(|error| error.to_string())?;
    if !metadata.is_file() {
        return Err("path is not a regular file".to_string());
    }
    if metadata.len() > max_file_bytes as u64 {
        return Err(format!(
            "file exceeds the configured per-file limit {max_file_bytes}"
        ));
    }
    let mut file = fs::File::open(path).map_err(|error| error.to_string())?;
    let mut bytes = Vec::new();
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    let mut buffer = [0_u8; 64 * 1024];
    let mut byte_count = 0usize;
    loop {
        if cancel.is_some_and(is_cancelled) {
            return Err(CANCELLATION_MESSAGE.to_string());
        }
        let read = file.read(&mut buffer).map_err(|error| error.to_string())?;
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
    })
}

fn is_harmless_include(source: &str) -> bool {
    let entries = directives(source);
    let mut content = source.to_string();
    for directive in entries.iter().rev() {
        content.replace_range(directive.start..directive.end, "");
    }
    content.chars().all(char::is_whitespace)
        && entries.iter().all(|directive| {
            matches!(
                directive.kind,
                DirectiveKind::CompilerDefine | DirectiveKind::MethodInfo | DirectiveKind::Harmless
            )
        })
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
    let Some(keyword) = body
        .trim_start()
        .split(|character: char| character.is_ascii_whitespace() || character == ':')
        .next()
    else {
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
    if keyword == "if" || keyword.starts_with("if") {
        return DirectiveKind::ConditionalStart;
    }
    if keyword == "define" || keyword == "undef" {
        return DirectiveKind::CompilerDefine;
    }
    if keyword == "methodinfo" {
        return DirectiveKind::MethodInfo;
    }
    if matches!(
        keyword.as_str(),
        "assertions"
            | "booleval"
            | "debug"
            | "debugsymbols"
            | "extendedsyntax"
            | "h"
            | "longstrings"
            | "m"
            | "optimization"
            | "overflowchecks"
            | "rangechecks"
            | "rtti"
            | "typedaddress"
            | "warn"
            | "writeableconst"
    ) {
        return DirectiveKind::Harmless;
    }
    DirectiveKind::Other
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
    use super::contains_any_identifier;

    #[test]
    fn contains_any_identifier_uses_identifier_boundaries() {
        let names = ["Foo".to_string(), "&Bar".to_string()];

        assert!(contains_any_identifier("value := FOO; &bar := 1;", &names));
        assert!(!contains_any_identifier("value := Foobar;", &names));
    }
}
