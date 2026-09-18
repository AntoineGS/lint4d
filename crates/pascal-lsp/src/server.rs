//! Synchronous stdio LSP protocol loop for the Pascal navigation workspace.

#[cfg(test)]
use crate::navigation::CompletionResolutionSeed;
use crate::navigation::{
    CompletionMetadata, CompletionOptions, CompletionResult, FOLDING_KIND_COMMENT,
    FOLDING_KIND_IMPORTS, FOLDING_KIND_REGION, FoldingRangeOptions,
};
use crate::workspace::codeactions::{self, ClientActionFeatures};
use crate::workspace::queries;
use crate::workspace::rename::{self, SourceRecord};
use crate::workspace::{
    FileChange, MAX_CONFIGURATION_WATCH_PATHS, NavigationState, Workspace, WorkspaceOptions,
    canonical_file_uri,
};
use crate::{NavigationIndex, NavigationTarget};
use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, bounded, unbounded};
use lsp_server::{Connection, ErrorCode, Message, Notification, Request, RequestId, Response};
use lsp_types::{
    ClientCapabilities, CodeAction, CodeActionOrCommand, CodeActionParams, CompletionItem,
    CompletionList, CompletionParams, CompletionResponse, DidChangeTextDocumentParams,
    DidChangeWatchedFilesParams, DidCloseTextDocumentParams, DidOpenTextDocumentParams,
    DidSaveTextDocumentParams, DocumentFormattingParams, DocumentHighlightParams, FileChangeType,
    FileSystemWatcher, FoldingRangeParams, GlobPattern, GotoDefinitionParams,
    GotoDefinitionResponse, HoverParams, InitializeParams, MarkupKind, OneOf, Position,
    PrepareRenameResponse, PublishDiagnosticsParams, ReferenceParams, Registration,
    RegistrationParams, RelativePattern, SelectionRangeParams, ServerInfo, SignatureHelpParams,
    TextDocumentIdentifier, Url, WatchKind, WorkspaceEdit, WorkspaceFolder,
};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::hash_map::RandomState;
use std::collections::{HashMap, HashSet, VecDeque};
use std::error::Error;
use std::hash::{BuildHasher, Hash, Hasher};
#[cfg(feature = "test-support")]
use std::io::Write;
use std::io::{self, BufRead, Read};
use std::mem::size_of;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const SERVER_NAME: &str = "pascal-lsp";
const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");
const MAX_PAYLOAD_BYTES: usize = 8 * 1024 * 1024;
const MAX_HEADER_BYTES: usize = 8 * 1024;
const MAX_ANALYSIS_JOBS: usize = 2;
/// Maximum number of accepted analysis requests waiting for a worker.
///
/// Queued requests retain only their parsed request parameters, never a
/// `WorkspaceInput`. This keeps source snapshot memory bounded by the number
/// of running workers rather than by the queue length.
const MAX_ANALYSIS_QUEUE: usize = 32;
// Keep one queue slot available for diagnostics while client work is busy.
const MAX_CLIENT_ANALYSIS_QUEUE: usize = MAX_ANALYSIS_QUEUE.saturating_sub(1);
/// Maximum number of client request recipients retained across running and
/// queued computations, including coalesced requests attached to one
/// computation.
const MAX_CLIENT_ANALYSIS_RECIPIENTS: usize = MAX_ANALYSIS_JOBS + MAX_CLIENT_ANALYSIS_QUEUE;
const MAX_INTERACTIVE_BURST: usize = 3;
const MAX_DIAGNOSTIC_BURST: usize = 2;
const ANALYSIS_POLL_INTERVAL: Duration = Duration::from_millis(10);
const ANALYSIS_SHUTDOWN_TIMEOUT: Duration = Duration::from_millis(100);
const MAX_WATCHER_REGISTRATION_RETRIES: usize = 3;
const ANALYSIS_QUEUE_FULL_MESSAGE: &str = "analysis queue is full; retry the request";
const ANALYSIS_SUPERSEDED_MESSAGE: &str = "request superseded by a newer document version";
const MAX_COMPLETION_RESOLUTION_ENTRIES: usize = 2_048;
const MAX_COMPLETION_RESOLUTION_BYTES: usize = 8 * 1024 * 1024;
const MAX_COMPLETION_RESOLUTION_DATA_BYTES: usize = 512;
const MAX_COMPLETION_RESOLUTION_RECORDS: usize = 1_024;
const MAX_COMPLETION_RESOLUTION_CONTEXT_BYTES: usize = 2 * 1024 * 1024;
const MAX_COMPLETION_RESOLUTION_ITEM_BYTES: usize = 64 * 1024;
const COMPLETION_RESOLUTION_DATA_VERSION: u8 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AnalysisPriority {
    Interactive,
    Diagnostics,
    Bulk,
}

impl AnalysisPriority {
    fn for_request(request: &AnalysisRequest) -> Self {
        match request {
            AnalysisRequest::Hover { .. }
            | AnalysisRequest::Completion { .. }
            | AnalysisRequest::SignatureHelp { .. }
            | AnalysisRequest::Navigation { .. }
            | AnalysisRequest::TypeDefinitions { .. }
            | AnalysisRequest::Prepare { .. }
            | AnalysisRequest::CodeActions(_)
            | AnalysisRequest::Resolve(_)
            | AnalysisRequest::ResolveCompletion(_)
            | AnalysisRequest::DocumentHighlights { .. }
            | AnalysisRequest::SelectionRanges { .. } => Self::Interactive,
            AnalysisRequest::Diagnostics { .. } => Self::Diagnostics,
            AnalysisRequest::Formatting { .. }
            | AnalysisRequest::DocumentSymbols { .. }
            | AnalysisRequest::WorkspaceSymbols { .. }
            | AnalysisRequest::References { .. }
            | AnalysisRequest::Rename { .. }
            | AnalysisRequest::SemanticTokens { .. }
            | AnalysisRequest::FoldingRanges { .. } => Self::Bulk,
        }
    }
}

/// A small weighted priority queue used by the analysis dispatcher.
///
/// Each priority is FIFO. Interactive requests are preferred, but after a
/// bounded burst one diagnostic or bulk request is selected. Diagnostics also
/// yield to bulk work after a short bounded burst when both lower-priority
/// classes are continuously populated. This gives every non-empty class a
/// finite service bound without making normal interactive requests wait behind
/// a bulk scan.
struct PriorityQueue<T> {
    interactive: VecDeque<T>,
    diagnostics: VecDeque<T>,
    bulk: VecDeque<T>,
    interactive_burst: usize,
    diagnostic_burst: usize,
}

impl<T> PriorityQueue<T> {
    fn new() -> Self {
        Self {
            interactive: VecDeque::new(),
            diagnostics: VecDeque::new(),
            bulk: VecDeque::new(),
            interactive_burst: 0,
            diagnostic_burst: 0,
        }
    }

    fn push(&mut self, priority: AnalysisPriority, item: T) {
        match priority {
            AnalysisPriority::Interactive => self.interactive.push_back(item),
            AnalysisPriority::Diagnostics => self.diagnostics.push_back(item),
            AnalysisPriority::Bulk => self.bulk.push_back(item),
        }
    }

    fn len(&self) -> usize {
        self.interactive
            .len()
            .saturating_add(self.diagnostics.len())
            .saturating_add(self.bulk.len())
    }

    fn is_empty(&self) -> bool {
        self.interactive.is_empty() && self.diagnostics.is_empty() && self.bulk.is_empty()
    }

    fn iter(&self) -> impl Iterator<Item = &T> {
        self.interactive
            .iter()
            .chain(self.diagnostics.iter())
            .chain(self.bulk.iter())
    }

    fn pop(&mut self) -> Option<T> {
        let lower_pending = !self.diagnostics.is_empty() || !self.bulk.is_empty();
        let priority = if !self.interactive.is_empty()
            && (!lower_pending || self.interactive_burst < MAX_INTERACTIVE_BURST)
        {
            AnalysisPriority::Interactive
        } else if !self.diagnostics.is_empty()
            && (self.bulk.is_empty() || self.diagnostic_burst < MAX_DIAGNOSTIC_BURST)
        {
            AnalysisPriority::Diagnostics
        } else if !self.bulk.is_empty() {
            AnalysisPriority::Bulk
        } else {
            AnalysisPriority::Interactive
        };

        let item = match priority {
            AnalysisPriority::Interactive => self.interactive.pop_front(),
            AnalysisPriority::Diagnostics => self.diagnostics.pop_front(),
            AnalysisPriority::Bulk => self.bulk.pop_front(),
        };
        if item.is_some() {
            match priority {
                AnalysisPriority::Interactive => {
                    if lower_pending {
                        self.interactive_burst = self.interactive_burst.saturating_add(1);
                    } else {
                        self.interactive_burst = 0;
                    }
                }
                AnalysisPriority::Diagnostics => {
                    self.interactive_burst = 0;
                    if !self.bulk.is_empty() {
                        self.diagnostic_burst = self.diagnostic_burst.saturating_add(1);
                    } else {
                        self.diagnostic_burst = 0;
                    }
                }
                AnalysisPriority::Bulk => {
                    self.interactive_burst = 0;
                    self.diagnostic_burst = 0;
                }
            }
        }
        item
    }

    fn remove_first(&mut self, mut predicate: impl FnMut(&T) -> bool) -> Option<T> {
        for queue in [&mut self.interactive, &mut self.diagnostics, &mut self.bulk] {
            let Some(index) = queue.iter().position(&mut predicate) else {
                continue;
            };
            return queue.remove(index);
        }
        None
    }

    fn find_mut(&mut self, mut predicate: impl FnMut(&T) -> bool) -> Option<&mut T> {
        for queue in [&mut self.interactive, &mut self.diagnostics, &mut self.bulk] {
            if let Some(item) = queue.iter_mut().find(|item| predicate(&**item)) {
                return Some(item);
            }
        }
        None
    }
}

impl<T> Default for PriorityQueue<T> {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Copy)]
enum TestBarrier {
    Navigation,
    Formatting,
    Diagnostics,
    Selection,
    CompletionResolution,
}

#[cfg(feature = "test-support")]
#[derive(Clone, Debug, Default)]
pub struct TestBarrierConfig {
    navigation: Option<TestBarrierPaths>,
    formatting: Option<TestBarrierPaths>,
    diagnostics: Option<TestBarrierPaths>,
    selection: Option<TestBarrierPaths>,
    completion_resolution: Option<TestBarrierPaths>,
    dispatch: Option<PathBuf>,
}

#[cfg(feature = "test-support")]
#[derive(Clone, Debug)]
struct TestBarrierPaths {
    entered: PathBuf,
    release: PathBuf,
}

#[cfg(feature = "test-support")]
impl TestBarrierConfig {
    pub fn new(
        navigation: Option<(PathBuf, PathBuf)>,
        formatting: Option<(PathBuf, PathBuf)>,
        diagnostics: Option<(PathBuf, PathBuf)>,
    ) -> Self {
        Self {
            navigation: navigation.map(|(entered, release)| TestBarrierPaths { entered, release }),
            formatting: formatting.map(|(entered, release)| TestBarrierPaths { entered, release }),
            diagnostics: diagnostics
                .map(|(entered, release)| TestBarrierPaths { entered, release }),
            selection: None,
            completion_resolution: None,
            dispatch: None,
        }
    }

    pub fn with_selection(mut self, selection: Option<(PathBuf, PathBuf)>) -> Self {
        self.selection = selection.map(|(entered, release)| TestBarrierPaths { entered, release });
        self
    }

    pub fn with_completion_resolution(
        mut self,
        completion_resolution: Option<(PathBuf, PathBuf)>,
    ) -> Self {
        self.completion_resolution =
            completion_resolution.map(|(entered, release)| TestBarrierPaths { entered, release });
        self
    }

    pub fn with_dispatch(mut self, path: Option<PathBuf>) -> Self {
        self.dispatch = path;
        self
    }

    fn record_dispatch(&self, priority: AnalysisPriority) {
        let Some(path) = self.dispatch.as_ref() else {
            return;
        };
        let marker = match priority {
            AnalysisPriority::Interactive => b'I',
            AnalysisPriority::Diagnostics => b'D',
            AnalysisPriority::Bulk => b'B',
        };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(path)
        {
            let _ = file.write_all(&[marker]);
        }
    }

    fn paths(&self, barrier: TestBarrier) -> Option<&TestBarrierPaths> {
        match barrier {
            TestBarrier::Navigation => self.navigation.as_ref(),
            TestBarrier::Formatting => self.formatting.as_ref(),
            TestBarrier::Diagnostics => self.diagnostics.as_ref(),
            TestBarrier::Selection => self.selection.as_ref(),
            TestBarrier::CompletionResolution => self.completion_resolution.as_ref(),
        }
    }
}

#[cfg(not(feature = "test-support"))]
impl TestBarrierConfig {
    fn record_dispatch(&self, _priority: AnalysisPriority) {}
}

#[cfg(not(feature = "test-support"))]
#[derive(Clone, Debug)]
struct TestBarrierConfig;

impl TestBarrierConfig {
    fn disabled() -> Self {
        #[cfg(feature = "test-support")]
        {
            Self::default()
        }
        #[cfg(not(feature = "test-support"))]
        {
            Self
        }
    }
}

#[cfg(feature = "test-support")]
fn wait_at_test_barrier(
    barrier: TestBarrier,
    config: &TestBarrierConfig,
    cancel: &AtomicBool,
) -> Result<(), String> {
    let Some(paths) = config.paths(barrier) else {
        return Ok(());
    };
    let mut marker = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(&paths.entered)
        .map_err(|error| format!("could not enter test barrier: {error}"))?;
    marker
        .write_all(b"x")
        .map_err(|error| format!("could not record test barrier entry: {error}"))?;
    drop(marker);
    loop {
        if cancel.load(std::sync::atomic::Ordering::Relaxed) {
            return Err(rename::CANCELLATION_MESSAGE.to_string());
        }
        if paths.release.exists() {
            return Ok(());
        }
        thread::sleep(ANALYSIS_POLL_INTERVAL);
    }
}

#[cfg(not(feature = "test-support"))]
fn wait_at_test_barrier(
    _barrier: TestBarrier,
    _config: &TestBarrierConfig,
    _cancel: &AtomicBool,
) -> Result<(), String> {
    Ok(())
}

#[derive(Debug, Clone, Copy)]
struct ClientFeatures {
    action_resolve: bool,
    action_disabled: bool,
    document_changes: bool,
    hierarchical_document_symbols: bool,
    hover_format: DocumentationFormat,
    completion_format: DocumentationFormat,
    completion_resolve_documentation: bool,
    completion_resolve_detail: bool,
    signature_help_format: DocumentationFormat,
    folding_range_limit: Option<usize>,
    line_folding_only: bool,
    folding_range_kind_value_set: Option<u8>,
}

#[derive(Debug, Clone)]
struct CompletionResolutionContext {
    source_uri: Url,
    position: Position,
    source_generation: u64,
    configuration_generation: u64,
    format: MarkupKind,
    resolve_documentation: bool,
    resolve_detail: bool,
    records: Vec<SourceRecord>,
}

#[derive(Debug, Clone)]
struct CompletionResolutionRequest {
    token: String,
    context: Arc<CompletionResolutionContext>,
    candidate_uri: Url,
    candidate_index: usize,
}

#[derive(Debug, Clone)]
struct CompletionAnalysis {
    uri: Url,
    position: Position,
    format: MarkupKind,
    resolve_documentation: bool,
    resolve_detail: bool,
    value: Result<CompletionResult, String>,
}

#[derive(Debug, Clone)]
struct CompletionResolutionAnalysis {
    token: String,
    value: Result<CompletionMetadata, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(rename_all = "camelCase")]
struct CompletionResolveData {
    version: u8,
    token: String,
    proof: String,
}

#[derive(Debug)]
struct CompletionResolutionEntry {
    context_id: u64,
    candidate_uri: Url,
    candidate_index: usize,
    proof: String,
    item: CompletionItem,
    bytes: usize,
}

#[derive(Debug)]
struct StoredCompletionResolutionContext {
    context: Arc<CompletionResolutionContext>,
    references: usize,
    bytes: usize,
}

#[derive(Debug)]
struct CompletionResolutionStore {
    hasher: RandomState,
    next_context_id: u64,
    entries: HashMap<String, CompletionResolutionEntry>,
    order: VecDeque<String>,
    contexts: HashMap<u64, StoredCompletionResolutionContext>,
    retained_bytes: usize,
}

impl CompletionResolutionStore {
    fn new() -> Self {
        Self {
            hasher: RandomState::new(),
            next_context_id: 0,
            entries: HashMap::new(),
            order: VecDeque::new(),
            contexts: HashMap::new(),
            retained_bytes: 0,
        }
    }

    fn issue_digest(&self, context_id: u64, item_index: usize, discriminator: u8) -> u64 {
        let mut hasher = self.hasher.build_hasher();
        discriminator.hash(&mut hasher);
        context_id.hash(&mut hasher);
        item_index.hash(&mut hasher);
        self.entries.len().hash(&mut hasher);
        hasher.finish()
    }

    fn token_and_proof(&self, context_id: u64, item_index: usize) -> (String, String) {
        let token = format!(
            "{:016x}{:016x}",
            self.issue_digest(context_id, item_index, 1),
            self.issue_digest(context_id, item_index, 2)
        );
        let mut hasher = self.hasher.build_hasher();
        token.hash(&mut hasher);
        context_id.hash(&mut hasher);
        item_index.hash(&mut hasher);
        let proof = format!("{:016x}", hasher.finish());
        (token, proof)
    }

    fn register(
        &mut self,
        analysis: CompletionAnalysis,
        source_generation: u64,
        configuration_generation: u64,
        records: &[SourceRecord],
    ) -> Result<CompletionList, String> {
        let CompletionAnalysis {
            uri,
            position,
            format,
            resolve_documentation,
            resolve_detail,
            value,
        } = analysis;
        let result = value?;
        if !resolve_documentation && !resolve_detail {
            return Ok(result.list);
        }
        if result.list.items.is_empty() {
            return Ok(result.list);
        }
        if result.list.items.len() != result.seeds.len() {
            return Err("completion result lost its exact resolution identities".to_string());
        }
        if result.list.items.len() > MAX_COMPLETION_RESOLUTION_ENTRIES {
            return Err("completion result exceeds the bounded resolution entry limit".to_string());
        }

        let (compact_records, context_bytes) = compact_completion_records(records)?;
        if compact_records.len() > MAX_COMPLETION_RESOLUTION_RECORDS {
            return Err("completion dependency observations exceed the bounded limit".to_string());
        }
        if context_bytes > MAX_COMPLETION_RESOLUTION_CONTEXT_BYTES {
            return Err(
                "completion dependency observations exceed the bounded byte limit".to_string(),
            );
        }
        let context_id = self
            .next_context_id
            .checked_add(1)
            .ok_or_else(|| "completion resolution context ID space exhausted".to_string())?;
        let context = Arc::new(CompletionResolutionContext {
            source_uri: canonical_file_uri(&uri),
            position,
            source_generation,
            configuration_generation,
            format,
            resolve_documentation,
            resolve_detail,
            records: compact_records,
        });

        let mut pending = Vec::with_capacity(result.list.items.len());
        let mut output = result.list;
        let mut new_bytes = context_bytes.saturating_add(size_of::<CompletionResolutionContext>());
        for (index, (item, seed)) in output.items.iter_mut().zip(result.seeds).enumerate() {
            let mut stored_item = item.clone();
            stored_item.data = None;
            let item_bytes = serde_json::to_vec(&stored_item)
                .map_err(|error| format!("could not size completion resolve item: {error}"))?
                .len();
            if item_bytes > MAX_COMPLETION_RESOLUTION_ITEM_BYTES {
                return Err("completion resolve item exceeds its bounded size".to_string());
            }
            let (token, proof) = self.token_and_proof(context_id, index);
            let data = CompletionResolveData {
                version: COMPLETION_RESOLUTION_DATA_VERSION,
                token: token.clone(),
                proof: proof.clone(),
            };
            let data_value = serde_json::to_value(&data)
                .map_err(|error| format!("could not encode completion resolve data: {error}"))?;
            let data_bytes = serde_json::to_vec(&data_value)
                .map_err(|error| format!("could not size completion resolve data: {error}"))?
                .len();
            if data_bytes > MAX_COMPLETION_RESOLUTION_DATA_BYTES {
                return Err("completion resolve data exceeds its bounded size".to_string());
            }
            let candidate_uri = canonical_file_uri(seed.candidate_uri());
            if self.entries.contains_key(&token)
                || pending.iter().any(|(existing, _)| existing == &token)
            {
                return Err("completion resolution token collision".to_string());
            }
            let token_storage = size_of::<String>().saturating_add(token.capacity());
            let entry_bytes = size_of::<CompletionResolutionEntry>()
                .saturating_add(item_bytes)
                .saturating_add(data_bytes)
                .saturating_add(token_storage.saturating_mul(2))
                .saturating_add(proof.capacity())
                .saturating_add(candidate_uri.as_str().len());
            new_bytes = new_bytes.saturating_add(entry_bytes);
            stored_item.data = Some(data_value);
            *item = stored_item.clone();
            pending.push((
                token,
                CompletionResolutionEntry {
                    context_id,
                    candidate_uri,
                    candidate_index: seed.candidate_index(),
                    proof,
                    item: stored_item,
                    bytes: entry_bytes,
                },
            ));
        }
        if new_bytes > MAX_COMPLETION_RESOLUTION_BYTES {
            return Err("completion resolution state exceeds its bounded byte limit".to_string());
        }
        while self.entries.len().saturating_add(pending.len()) > MAX_COMPLETION_RESOLUTION_ENTRIES
            || self.retained_bytes.saturating_add(new_bytes) > MAX_COMPLETION_RESOLUTION_BYTES
        {
            let Some(victim) = self.order.pop_front() else {
                return Err("completion resolution state could not make room".to_string());
            };
            self.remove(&victim);
        }
        self.next_context_id = context_id;
        self.retained_bytes = self.retained_bytes.saturating_add(new_bytes);
        self.contexts.insert(
            context_id,
            StoredCompletionResolutionContext {
                context,
                references: pending.len(),
                bytes: context_bytes.saturating_add(size_of::<CompletionResolutionContext>()),
            },
        );
        for (token, entry) in pending {
            self.order.push_back(token.clone());
            self.entries.insert(token, entry);
        }
        Ok(output)
    }

    fn remove(&mut self, token: &str) {
        let Some(entry) = self.entries.remove(token) else {
            return;
        };
        self.retained_bytes = self.retained_bytes.saturating_sub(entry.bytes);
        if let Some(context) = self.contexts.get_mut(&entry.context_id) {
            context.references = context.references.saturating_sub(1);
            if context.references == 0 {
                let context = self
                    .contexts
                    .remove(&entry.context_id)
                    .expect("completion context exists while removing its final entry");
                self.retained_bytes = self.retained_bytes.saturating_sub(context.bytes);
            }
        }
    }

    fn request(&self, item: &CompletionItem) -> Result<CompletionResolutionRequest, String> {
        let value = item
            .data
            .as_ref()
            .ok_or_else(|| "completion item has no resolve data".to_string())?;
        let data_bytes = serde_json::to_vec(value)
            .map_err(|error| format!("invalid completion resolve data: {error}"))?
            .len();
        if data_bytes > MAX_COMPLETION_RESOLUTION_DATA_BYTES {
            return Err("completion resolve data exceeds its bounded size".to_string());
        }
        let data: CompletionResolveData = serde_json::from_value(value.clone())
            .map_err(|error| format!("invalid completion resolve data: {error}"))?;
        if data.version != COMPLETION_RESOLUTION_DATA_VERSION
            || data.token.len() > 64
            || data.proof.len() > 32
        {
            return Err("completion resolve data is stale or tampered".to_string());
        }
        let entry = self
            .entries
            .get(&data.token)
            .ok_or_else(|| "completion resolve data is stale, foreign, or evicted".to_string())?;
        if entry.proof != data.proof {
            return Err("completion resolve data is stale or tampered".to_string());
        }
        let context = self
            .contexts
            .get(&entry.context_id)
            .ok_or_else(|| "completion resolve context was evicted".to_string())?;
        Ok(CompletionResolutionRequest {
            token: data.token,
            context: Arc::clone(&context.context),
            candidate_uri: entry.candidate_uri.clone(),
            candidate_index: entry.candidate_index,
        })
    }

    fn finish(&self, token: &str, metadata: CompletionMetadata) -> Result<CompletionItem, String> {
        let entry = self
            .entries
            .get(token)
            .ok_or_else(|| "completion resolve data was evicted while resolving".to_string())?;
        let mut item = entry.item.clone();
        let context = self
            .contexts
            .get(&entry.context_id)
            .ok_or_else(|| "completion resolve context was evicted while resolving".to_string())?;
        if context.context.resolve_documentation {
            item.documentation = metadata.documentation;
        }
        if context.context.resolve_detail {
            item.detail = metadata.detail;
        }
        Ok(item)
    }
}

impl Default for CompletionResolutionStore {
    fn default() -> Self {
        Self::new()
    }
}

fn compact_completion_records(
    records: &[SourceRecord],
) -> Result<(Vec<SourceRecord>, usize), String> {
    let mut compact = Vec::with_capacity(records.len());
    let mut bytes = 0usize;
    for record in records {
        let content_hash = record.content_hash.or_else(|| {
            if record.open {
                Some(crate::workspace::rename::text_content_hash(&record.text))
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
            return Err(format!(
                "completion dependency {} has no bounded content observation",
                record.uri
            ));
        }
        let observation = SourceRecord {
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
            read_policy: record.read_policy.clone(),
            path_entry: record.path_entry.clone(),
            include_payload: record.include_payload,
            missing_provider_candidate: record.missing_provider_candidate,
            missing_provider_scope: record.missing_provider_scope.clone(),
            auto_import_provider_observation: record.auto_import_provider_observation,
            auto_import_scopes: record.auto_import_scopes.clone(),
        };
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

fn compact_completion_record_bytes(record: &SourceRecord) -> usize {
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DocumentationFormat {
    PlainText,
    Markdown,
}

impl DocumentationFormat {
    fn markup_kind(self) -> MarkupKind {
        match self {
            Self::PlainText => MarkupKind::PlainText,
            Self::Markdown => MarkupKind::Markdown,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PositionRequestParams {
    text_document: TextDocumentIdentifier,
    position: Position,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RenameRequestParams {
    text_document: TextDocumentIdentifier,
    position: Position,
    new_name: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProjectContextRequestParams {
    text_document: TextDocumentIdentifier,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SelectProjectRequestParams {
    text_document: TextDocumentIdentifier,
    project_uri: Value,
}

#[derive(Debug)]
enum AnalysisRequest {
    Hover {
        uri: Url,
        position: Position,
        format: MarkupKind,
    },
    Completion {
        uri: Url,
        position: Position,
        format: MarkupKind,
        resolve_documentation: bool,
        resolve_detail: bool,
    },
    SignatureHelp {
        uri: Url,
        position: Position,
        format: MarkupKind,
    },
    Navigation {
        uri: Url,
        position: Position,
        target: NavigationTarget,
    },
    Formatting {
        uri: Url,
    },
    Diagnostics {
        uri: Url,
    },
    TypeDefinitions {
        uri: Url,
        position: Position,
    },
    Prepare {
        uri: Url,
        position: Position,
    },
    Rename {
        uri: Url,
        position: Position,
        new_name: String,
    },
    CodeActions(CodeActionParams),
    Resolve(CodeAction),
    ResolveCompletion(CompletionResolutionRequest),
    DocumentSymbols {
        uri: Url,
        hierarchical: bool,
    },
    WorkspaceSymbols {
        query: String,
    },
    References {
        uri: Url,
        position: Position,
        include_declaration: bool,
    },
    DocumentHighlights {
        uri: Url,
        position: Position,
    },
    SelectionRanges {
        uri: Url,
        positions: Vec<Position>,
    },
    SemanticTokens {
        uri: Url,
        range: Option<lsp_types::Range>,
    },
    FoldingRanges {
        uri: Url,
    },
}

#[derive(Clone)]
enum AnalysisResultValue {
    Hover(Result<Option<lsp_types::Hover>, String>),
    Completion(CompletionAnalysis),
    ResolveCompletion(CompletionResolutionAnalysis),
    SignatureHelp(Result<Option<lsp_types::SignatureHelp>, String>),
    Navigation(NavigationAnalysis),
    Formatting(Result<Option<lsp_types::TextEdit>, String>),
    Diagnostics(DiagnosticsAnalysis),
    TypeDefinitions(Result<Vec<lsp_types::Location>, String>),
    Prepare(Result<PrepareRenameResponse, String>),
    Rename(Box<Result<WorkspaceEdit, String>>),
    CodeActions(Result<Vec<CodeActionOrCommand>, String>),
    Resolve(Box<Result<CodeAction, String>>),
    DocumentSymbols {
        uri: Url,
        hierarchical: bool,
        value: Result<Vec<lsp_types::DocumentSymbol>, String>,
    },
    WorkspaceSymbols(Result<Vec<lsp_types::SymbolInformation>, String>),
    References(Result<Vec<lsp_types::Location>, String>),
    DocumentHighlights(Result<Vec<lsp_types::DocumentHighlight>, String>),
    SelectionRanges(Result<Vec<lsp_types::SelectionRange>, String>),
    SemanticTokens(Result<lsp_types::SemanticTokens, String>),
    FoldingRanges(Result<Vec<lsp_types::FoldingRange>, String>),
}

#[derive(Clone)]
struct NavigationAnalysis {
    value: Result<Vec<lsp_types::Location>, String>,
    state: Option<NavigationState>,
}

#[derive(Clone)]
struct DiagnosticsAnalysis {
    uri: Url,
    version: Option<i32>,
    value: Result<Vec<lsp_types::Diagnostic>, String>,
    discard: bool,
}

#[derive(Debug, Default)]
struct DiagnosticNotificationEffect {
    refresh: Vec<Url>,
    cancel: Vec<Url>,
}

impl DiagnosticNotificationEffect {
    fn refresh_uri(&mut self, uri: Url) {
        if !self.refresh.contains(&uri) {
            self.refresh.push(uri);
        }
    }

    fn cancel_uri(&mut self, uri: Url) {
        if !self.cancel.contains(&uri) {
            self.cancel.push(uri);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct AnalysisComputationId(u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum AnalysisJobId {
    Client(AnalysisComputationId),
    Diagnostic(AnalysisComputationId),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum NavigationObservationTarget {
    Declaration,
    Definition,
    Implementation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum ObservationMethod {
    Hover {
        markdown: bool,
    },
    Completion {
        markdown: bool,
        resolve_documentation: bool,
        resolve_detail: bool,
    },
    SignatureHelp {
        markdown: bool,
    },
    Navigation(NavigationObservationTarget),
    TypeDefinitions,
    Prepare,
    DocumentSymbols {
        hierarchical: bool,
    },
    WorkspaceSymbols,
    References {
        include_declaration: bool,
    },
    DocumentHighlights,
    SelectionRanges,
    SemanticTokens {
        range: Option<ObservationRange>,
    },
    FoldingRanges,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct ObservationPosition {
    line: u32,
    character: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct ObservationRange {
    start: ObservationPosition,
    end: ObservationPosition,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ObservationKey {
    method: ObservationMethod,
    uri: Option<Url>,
    position: Option<ObservationPosition>,
    positions: Option<Vec<ObservationPosition>>,
    query: Option<String>,
    version: Option<i32>,
    source_generation: u64,
    configuration_generation: u64,
}

impl ObservationKey {
    fn for_request(request: &AnalysisRequest, workspace: &Workspace) -> Option<Self> {
        let (method, uri, position, positions, query) = match request {
            AnalysisRequest::Hover {
                uri,
                position,
                format,
            } => (
                ObservationMethod::Hover {
                    markdown: matches!(format, MarkupKind::Markdown),
                },
                Some(uri.clone()),
                Some(ObservationPosition {
                    line: position.line,
                    character: position.character,
                }),
                None,
                None,
            ),
            AnalysisRequest::Completion {
                uri,
                position,
                format,
                resolve_documentation,
                resolve_detail,
            } => (
                ObservationMethod::Completion {
                    markdown: matches!(format, MarkupKind::Markdown),
                    resolve_documentation: *resolve_documentation,
                    resolve_detail: *resolve_detail,
                },
                Some(uri.clone()),
                Some(ObservationPosition {
                    line: position.line,
                    character: position.character,
                }),
                None,
                None,
            ),
            AnalysisRequest::SignatureHelp {
                uri,
                position,
                format,
            } => (
                ObservationMethod::SignatureHelp {
                    markdown: matches!(format, MarkupKind::Markdown),
                },
                Some(uri.clone()),
                Some(ObservationPosition {
                    line: position.line,
                    character: position.character,
                }),
                None,
                None,
            ),
            AnalysisRequest::Navigation {
                uri,
                position,
                target,
            } => (
                ObservationMethod::Navigation(match target {
                    NavigationTarget::Declaration => NavigationObservationTarget::Declaration,
                    NavigationTarget::Definition => NavigationObservationTarget::Definition,
                    NavigationTarget::Implementation => NavigationObservationTarget::Implementation,
                }),
                Some(uri.clone()),
                Some(ObservationPosition {
                    line: position.line,
                    character: position.character,
                }),
                None,
                None,
            ),
            AnalysisRequest::TypeDefinitions { uri, position } => (
                ObservationMethod::TypeDefinitions,
                Some(uri.clone()),
                Some(ObservationPosition {
                    line: position.line,
                    character: position.character,
                }),
                None,
                None,
            ),
            AnalysisRequest::Prepare { uri, position } => (
                ObservationMethod::Prepare,
                Some(uri.clone()),
                Some(ObservationPosition {
                    line: position.line,
                    character: position.character,
                }),
                None,
                None,
            ),
            AnalysisRequest::DocumentSymbols { uri, hierarchical } => (
                ObservationMethod::DocumentSymbols {
                    hierarchical: *hierarchical,
                },
                Some(uri.clone()),
                None,
                None,
                None,
            ),
            AnalysisRequest::WorkspaceSymbols { query } => (
                ObservationMethod::WorkspaceSymbols,
                None,
                None,
                None,
                Some(query.clone()),
            ),
            AnalysisRequest::References {
                uri,
                position,
                include_declaration,
            } => (
                ObservationMethod::References {
                    include_declaration: *include_declaration,
                },
                Some(uri.clone()),
                Some(ObservationPosition {
                    line: position.line,
                    character: position.character,
                }),
                None,
                None,
            ),
            AnalysisRequest::DocumentHighlights { uri, position } => (
                ObservationMethod::DocumentHighlights,
                Some(uri.clone()),
                Some(ObservationPosition {
                    line: position.line,
                    character: position.character,
                }),
                None,
                None,
            ),
            AnalysisRequest::SemanticTokens { uri, range } => (
                ObservationMethod::SemanticTokens {
                    range: range.as_ref().map(|range| ObservationRange {
                        start: ObservationPosition {
                            line: range.start.line,
                            character: range.start.character,
                        },
                        end: ObservationPosition {
                            line: range.end.line,
                            character: range.end.character,
                        },
                    }),
                },
                Some(uri.clone()),
                None,
                None,
                None,
            ),
            AnalysisRequest::FoldingRanges { uri } => (
                ObservationMethod::FoldingRanges,
                Some(uri.clone()),
                None,
                None,
                None,
            ),
            AnalysisRequest::SelectionRanges { uri, positions } => (
                ObservationMethod::SelectionRanges,
                Some(uri.clone()),
                None,
                Some(
                    positions
                        .iter()
                        .map(|position| ObservationPosition {
                            line: position.line,
                            character: position.character,
                        })
                        .collect(),
                ),
                None,
            ),
            AnalysisRequest::Formatting { .. }
            | AnalysisRequest::Diagnostics { .. }
            | AnalysisRequest::Rename { .. }
            | AnalysisRequest::CodeActions(_)
            | AnalysisRequest::Resolve(_)
            | AnalysisRequest::ResolveCompletion(_) => return None,
        };
        let version = uri.as_ref().and_then(|uri| workspace.document_version(uri));
        Some(Self {
            method,
            uri,
            position,
            positions,
            query,
            version,
            source_generation: workspace.source_generation(),
            configuration_generation: workspace.configuration_generation(),
        })
    }

    fn same_query(&self, other: &Self) -> bool {
        self.method == other.method
            && self.uri == other.uri
            && self.position == other.position
            && self.positions == other.positions
            && self.query == other.query
    }

    fn is_superseded_by(&self, newer: &Self) -> bool {
        self.same_query(newer)
            && matches!((self.version, newer.version), (Some(old), Some(new)) if new > old)
    }
}

#[derive(Debug)]
struct QueuedClientAnalysis {
    id: AnalysisComputationId,
    request: AnalysisRequest,
    features: ClientFeatures,
    client_ids: Vec<RequestId>,
    key: Option<ObservationKey>,
}

#[derive(Debug)]
struct QueuedDiagnostic {
    id: AnalysisComputationId,
    uri: Url,
}

#[derive(Debug)]
enum QueuedAnalysis {
    Client(QueuedClientAnalysis),
    Diagnostic(QueuedDiagnostic),
}

#[derive(Clone)]
struct AnalysisResult {
    id: AnalysisJobId,
    source_generation: u64,
    configuration_generation: u64,
    records: Vec<SourceRecord>,
    value: AnalysisResultValue,
}

struct PendingAnalysis {
    cancellation: Arc<AtomicBool>,
    handle: JoinHandle<()>,
    client_ids: Vec<RequestId>,
    key: Option<ObservationKey>,
}

struct PendingDiagnostic {
    uri: Url,
    analysis: PendingAnalysis,
}

struct DispatchFailure {
    client_ids: Vec<RequestId>,
    diagnostic: Option<QueuedDiagnostic>,
    message: String,
}

struct FileWatcherRegistration {
    acknowledged_paths: HashSet<String>,
    pending_paths: std::collections::HashMap<RequestId, HashSet<String>>,
    rejected_attempts: std::collections::HashMap<String, usize>,
    degraded_paths: HashSet<String>,
    relative_pattern_support: bool,
    next_id: usize,
}

impl FileWatcherRegistration {
    fn with_relative_pattern_support(relative_pattern_support: bool) -> Self {
        Self {
            acknowledged_paths: HashSet::new(),
            pending_paths: std::collections::HashMap::new(),
            rejected_attempts: std::collections::HashMap::new(),
            degraded_paths: HashSet::new(),
            relative_pattern_support,
            next_id: 1,
        }
    }

    fn new_paths(&mut self, paths: impl IntoIterator<Item = PathBuf>) -> Vec<PathBuf> {
        let pending_paths = self
            .pending_paths
            .values()
            .flat_map(|paths| paths.iter())
            .cloned()
            .collect::<HashSet<_>>();
        let tracked = self
            .acknowledged_paths
            .len()
            .saturating_add(pending_paths.len());
        let mut selected = Vec::new();
        let mut selected_keys = HashSet::new();
        for path in paths {
            let key = watcher_path_key(&path);
            if self.acknowledged_paths.contains(&key)
                || pending_paths.contains(&key)
                || self.degraded_paths.contains(&key)
                || !selected_keys.insert(key)
            {
                continue;
            }
            if !self.relative_pattern_support {
                self.degraded_paths.insert(watcher_path_key(&path));
                continue;
            }
            if tracked.saturating_add(selected.len()) >= MAX_CONFIGURATION_WATCH_PATHS {
                break;
            }
            selected.push(path);
        }
        selected
    }

    fn mark_pending(&mut self, request_id: RequestId, paths: &[PathBuf]) {
        let keys = paths
            .iter()
            .map(|path| watcher_path_key(path))
            .collect::<HashSet<_>>();
        if !keys.is_empty() {
            self.pending_paths.insert(request_id, keys);
        }
    }

    fn complete(&mut self, request_id: &RequestId, rejected: bool) -> bool {
        let Some(paths) = self.pending_paths.remove(request_id) else {
            return false;
        };
        if rejected {
            for path in paths {
                let attempts = self.rejected_attempts.entry(path.clone()).or_default();
                *attempts = attempts.saturating_add(1);
                if *attempts >= MAX_WATCHER_REGISTRATION_RETRIES {
                    self.degraded_paths.insert(path);
                }
            }
        } else {
            for path in paths {
                self.rejected_attempts.remove(&path);
                self.acknowledged_paths.insert(path);
            }
        }
        true
    }

    fn handle_response(&mut self, response: &Response) -> Option<bool> {
        if !self.pending_paths.contains_key(&response.id) {
            return None;
        }
        let rejected = response.error.is_some();
        self.complete(&response.id, rejected);
        Some(!rejected)
    }
}

struct AnalysisJobs {
    sender: Sender<AnalysisResult>,
    receiver: Receiver<AnalysisResult>,
    pending: std::collections::HashMap<AnalysisComputationId, PendingAnalysis>,
    diagnostics: HashMap<AnalysisComputationId, PendingDiagnostic>,
    queue: PriorityQueue<QueuedAnalysis>,
    request_to_job: HashMap<RequestId, AnalysisComputationId>,
    observation_jobs: HashMap<ObservationKey, AnalysisComputationId>,
    diagnostic_jobs: HashMap<Url, AnalysisComputationId>,
    completion_resolutions: CompletionResolutionStore,
    test_barriers: TestBarrierConfig,
    next_computation_id: u64,
    shutting_down: bool,
}

impl AnalysisJobs {
    #[cfg(test)]
    fn new() -> Self {
        Self::with_test_barriers(TestBarrierConfig::disabled())
    }

    fn with_test_barriers(test_barriers: TestBarrierConfig) -> Self {
        let (sender, receiver) = unbounded();
        Self {
            sender,
            receiver,
            pending: std::collections::HashMap::new(),
            diagnostics: HashMap::new(),
            queue: PriorityQueue::new(),
            request_to_job: HashMap::new(),
            observation_jobs: HashMap::new(),
            diagnostic_jobs: HashMap::new(),
            completion_resolutions: CompletionResolutionStore::new(),
            test_barriers,
            next_computation_id: 0,
            shutting_down: false,
        }
    }

    fn spawn(
        &self,
        id: AnalysisJobId,
        request: AnalysisRequest,
        workspace: &Workspace,
        features: ClientFeatures,
    ) -> Result<PendingAnalysis, String> {
        let input = workspace.analysis_input();
        let cancellation = Arc::new(AtomicBool::new(false));
        let source_generation = input.source_generation;
        let configuration_generation = input.configuration_generation;
        let worker_cancellation = Arc::clone(&cancellation);
        let test_barriers = self.test_barriers.clone();
        let sender = self.sender.clone();
        let worker_id = id;
        let panic_id = id;
        let panic_value = match &request {
            AnalysisRequest::Hover { .. } => AnalysisResultValue::Hover(Err(
                "analysis worker failed without changing workspace state".to_string(),
            )),
            AnalysisRequest::Completion {
                uri,
                position,
                format,
                resolve_documentation,
                resolve_detail,
            } => AnalysisResultValue::Completion(CompletionAnalysis {
                uri: uri.clone(),
                position: *position,
                format: format.clone(),
                resolve_documentation: *resolve_documentation,
                resolve_detail: *resolve_detail,
                value: Err("analysis worker failed without changing workspace state".to_string()),
            }),
            AnalysisRequest::SignatureHelp { .. } => AnalysisResultValue::SignatureHelp(Err(
                "analysis worker failed without changing workspace state".to_string(),
            )),
            AnalysisRequest::Navigation { .. } => {
                AnalysisResultValue::Navigation(NavigationAnalysis {
                    value: Err(
                        "analysis worker failed without changing workspace state".to_string()
                    ),
                    state: None,
                })
            }
            AnalysisRequest::Formatting { .. } => AnalysisResultValue::Formatting(Err(
                "analysis worker failed without changing workspace state".to_string(),
            )),
            AnalysisRequest::Diagnostics { uri } => {
                AnalysisResultValue::Diagnostics(DiagnosticsAnalysis {
                    uri: uri.clone(),
                    version: input.document_versions.get(uri).copied(),
                    value: Err(
                        "analysis worker failed without changing workspace state".to_string()
                    ),
                    discard: false,
                })
            }
            AnalysisRequest::TypeDefinitions { .. } => AnalysisResultValue::TypeDefinitions(Err(
                "analysis worker failed without changing workspace state".to_string(),
            )),
            AnalysisRequest::Prepare { .. } => AnalysisResultValue::Prepare(Err(
                "analysis worker failed without changing workspace state".to_string(),
            )),
            AnalysisRequest::Rename { .. } => AnalysisResultValue::Rename(Box::new(Err(
                "analysis worker failed without changing workspace state".to_string(),
            ))),
            AnalysisRequest::CodeActions(_) => AnalysisResultValue::CodeActions(Err(
                "analysis worker failed without changing workspace state".to_string(),
            )),
            AnalysisRequest::Resolve(_) => AnalysisResultValue::Resolve(Box::new(Err(
                "analysis worker failed without changing workspace state".to_string(),
            ))),
            AnalysisRequest::ResolveCompletion(request) => {
                AnalysisResultValue::ResolveCompletion(CompletionResolutionAnalysis {
                    token: request.token.clone(),
                    value: Err(
                        "analysis worker failed without changing workspace state".to_string()
                    ),
                })
            }
            AnalysisRequest::DocumentSymbols { uri, hierarchical } => {
                AnalysisResultValue::DocumentSymbols {
                    uri: uri.clone(),
                    hierarchical: *hierarchical,
                    value: Err(
                        "analysis worker failed without changing workspace state".to_string()
                    ),
                }
            }
            AnalysisRequest::WorkspaceSymbols { .. } => AnalysisResultValue::WorkspaceSymbols(Err(
                "analysis worker failed without changing workspace state".to_string(),
            )),
            AnalysisRequest::References { .. } => AnalysisResultValue::References(Err(
                "analysis worker failed without changing workspace state".to_string(),
            )),
            AnalysisRequest::DocumentHighlights { .. } => AnalysisResultValue::DocumentHighlights(
                Err("analysis worker failed without changing workspace state".to_string()),
            ),
            AnalysisRequest::SelectionRanges { .. } => AnalysisResultValue::SelectionRanges(Err(
                "analysis worker failed without changing workspace state".to_string(),
            )),
            AnalysisRequest::SemanticTokens { .. } => AnalysisResultValue::SemanticTokens(Err(
                "analysis worker failed without changing workspace state".to_string(),
            )),
            AnalysisRequest::FoldingRanges { .. } => AnalysisResultValue::FoldingRanges(Err(
                "analysis worker failed without changing workspace state".to_string(),
            )),
        };
        let handle = thread::Builder::new()
            .name("PascalLspAnalysis".to_string())
            .spawn(move || {
                let validation_input = input.clone();
                let result =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match request {
                        AnalysisRequest::Hover {
                            uri,
                            position,
                            format,
                        } => {
                            let computed = queries::hover_from_input(
                                input,
                                &uri,
                                position,
                                format,
                                &worker_cancellation,
                            );
                            AnalysisResult {
                                id: worker_id,
                                source_generation: computed.source_generation,
                                configuration_generation: computed.configuration_generation,
                                records: computed.records,
                                value: AnalysisResultValue::Hover(computed.value),
                            }
                        }
                        AnalysisRequest::Completion {
                            uri,
                            position,
                            format,
                            resolve_documentation,
                            resolve_detail,
                        } => {
                            let computed = queries::completion_from_input_with_options(
                                input,
                                &uri,
                                position,
                                CompletionOptions {
                                    format: format.clone(),
                                    defer_documentation: resolve_documentation,
                                    defer_detail: resolve_detail,
                                },
                                &worker_cancellation,
                            );
                            AnalysisResult {
                                id: worker_id,
                                source_generation: computed.source_generation,
                                configuration_generation: computed.configuration_generation,
                                records: computed.records,
                                value: AnalysisResultValue::Completion(CompletionAnalysis {
                                    uri,
                                    position,
                                    format,
                                    resolve_documentation,
                                    resolve_detail,
                                    value: computed.value,
                                }),
                            }
                        }
                        AnalysisRequest::SignatureHelp {
                            uri,
                            position,
                            format,
                        } => {
                            let computed = queries::signature_help_from_input_with_format(
                                input,
                                &uri,
                                position,
                                format,
                                &worker_cancellation,
                            );
                            AnalysisResult {
                                id: worker_id,
                                source_generation: computed.source_generation,
                                configuration_generation: computed.configuration_generation,
                                records: computed.records,
                                value: AnalysisResultValue::SignatureHelp(computed.value),
                            }
                        }
                        AnalysisRequest::Navigation {
                            uri,
                            position,
                            target,
                        } => {
                            if let Err(error) = wait_at_test_barrier(
                                TestBarrier::Navigation,
                                &test_barriers,
                                &worker_cancellation,
                            ) {
                                AnalysisResult {
                                    id: worker_id,
                                    source_generation,
                                    configuration_generation,
                                    records: Vec::new(),
                                    value: AnalysisResultValue::Navigation(NavigationAnalysis {
                                        value: Err(error),
                                        state: None,
                                    }),
                                }
                            } else {
                                let computed = queries::navigation_from_input(
                                    input,
                                    &uri,
                                    position,
                                    target,
                                    &worker_cancellation,
                                );
                                let (value, state) = match computed.value {
                                    Ok(result) => (Ok(result.locations), Some(result.state)),
                                    Err(error) => (Err(error), None),
                                };
                                AnalysisResult {
                                    id: worker_id,
                                    source_generation: computed.source_generation,
                                    configuration_generation: computed.configuration_generation,
                                    records: computed.records,
                                    value: AnalysisResultValue::Navigation(NavigationAnalysis {
                                        value,
                                        state,
                                    }),
                                }
                            }
                        }
                        AnalysisRequest::Formatting { uri } => {
                            if let Err(error) = wait_at_test_barrier(
                                TestBarrier::Formatting,
                                &test_barriers,
                                &worker_cancellation,
                            ) {
                                AnalysisResult {
                                    id: worker_id,
                                    source_generation,
                                    configuration_generation,
                                    records: Vec::new(),
                                    value: AnalysisResultValue::Formatting(Err(error)),
                                }
                            } else {
                                let computed = queries::formatting_from_input(
                                    input,
                                    &uri,
                                    &worker_cancellation,
                                );
                                AnalysisResult {
                                    id: worker_id,
                                    source_generation: computed.source_generation,
                                    configuration_generation: computed.configuration_generation,
                                    records: computed.records,
                                    value: AnalysisResultValue::Formatting(computed.value),
                                }
                            }
                        }
                        AnalysisRequest::Diagnostics { uri } => {
                            let version = validation_input.document_versions.get(&uri).copied();
                            if let Err(error) = wait_at_test_barrier(
                                TestBarrier::Diagnostics,
                                &test_barriers,
                                &worker_cancellation,
                            ) {
                                AnalysisResult {
                                    id: worker_id,
                                    source_generation,
                                    configuration_generation,
                                    records: Vec::new(),
                                    value: AnalysisResultValue::Diagnostics(DiagnosticsAnalysis {
                                        uri,
                                        version,
                                        value: Err(error),
                                        discard: false,
                                    }),
                                }
                            } else {
                                let computed = queries::diagnostics_from_input(
                                    input,
                                    &uri,
                                    &worker_cancellation,
                                );
                                let value = computed.value.map(|result| DiagnosticsAnalysis {
                                    uri: result.uri,
                                    version: result.version,
                                    value: Ok(result.diagnostics),
                                    discard: false,
                                });
                                AnalysisResult {
                                    id: worker_id,
                                    source_generation: computed.source_generation,
                                    configuration_generation: computed.configuration_generation,
                                    records: computed.records,
                                    value: match value {
                                        Ok(analysis) => AnalysisResultValue::Diagnostics(analysis),
                                        Err(error) => {
                                            AnalysisResultValue::Diagnostics(DiagnosticsAnalysis {
                                                uri,
                                                version,
                                                value: Err(error),
                                                discard: false,
                                            })
                                        }
                                    },
                                }
                            }
                        }
                        AnalysisRequest::TypeDefinitions { uri, position } => {
                            let computed = queries::type_definitions_from_input(
                                input,
                                &uri,
                                position,
                                &worker_cancellation,
                            );
                            AnalysisResult {
                                id: worker_id,
                                source_generation: computed.source_generation,
                                configuration_generation: computed.configuration_generation,
                                records: computed.records,
                                value: AnalysisResultValue::TypeDefinitions(computed.value),
                            }
                        }
                        AnalysisRequest::Prepare { uri, position } => {
                            let computed = rename::prepare_from_input(
                                input,
                                &uri,
                                position,
                                &worker_cancellation,
                            );
                            AnalysisResult {
                                id: worker_id,
                                source_generation: computed.source_generation,
                                configuration_generation: computed.configuration_generation,
                                records: computed.records,
                                value: AnalysisResultValue::Prepare(computed.value),
                            }
                        }
                        AnalysisRequest::Rename {
                            uri,
                            position,
                            new_name,
                        } => {
                            let computed = rename::rename_from_input(
                                input,
                                &uri,
                                position,
                                &new_name,
                                features.document_changes,
                                &worker_cancellation,
                            );
                            AnalysisResult {
                                id: worker_id,
                                source_generation: computed.source_generation,
                                configuration_generation: computed.configuration_generation,
                                records: computed.records,
                                value: AnalysisResultValue::Rename(Box::new(computed.value)),
                            }
                        }
                        AnalysisRequest::CodeActions(params) => {
                            let computed = codeactions::code_actions_from_input(
                                input,
                                params,
                                ClientActionFeatures {
                                    resolve: features.action_resolve,
                                    document_changes: features.document_changes,
                                    disabled: features.action_disabled,
                                },
                                &worker_cancellation,
                            );
                            AnalysisResult {
                                id: worker_id,
                                source_generation: computed.source_generation,
                                configuration_generation: computed.configuration_generation,
                                records: computed.records,
                                value: AnalysisResultValue::CodeActions(computed.value),
                            }
                        }
                        AnalysisRequest::Resolve(action) => {
                            let computed = codeactions::resolve_from_input(
                                input,
                                action,
                                ClientActionFeatures {
                                    resolve: features.action_resolve,
                                    document_changes: features.document_changes,
                                    disabled: features.action_disabled,
                                },
                                &worker_cancellation,
                            );
                            AnalysisResult {
                                id: worker_id,
                                source_generation: computed.source_generation,
                                configuration_generation: computed.configuration_generation,
                                records: computed.records,
                                value: AnalysisResultValue::Resolve(Box::new(computed.value)),
                            }
                        }
                        AnalysisRequest::ResolveCompletion(request) => {
                            let CompletionResolutionRequest {
                                token,
                                context,
                                candidate_uri,
                                candidate_index,
                            } = request;
                            if let Err(error) = wait_at_test_barrier(
                                TestBarrier::CompletionResolution,
                                &test_barriers,
                                &worker_cancellation,
                            ) {
                                AnalysisResult {
                                    id: worker_id,
                                    source_generation: context.source_generation,
                                    configuration_generation: context.configuration_generation,
                                    records: Vec::new(),
                                    value: AnalysisResultValue::ResolveCompletion(
                                        CompletionResolutionAnalysis {
                                            token,
                                            value: Err(error),
                                        },
                                    ),
                                }
                            } else {
                                let mut computed = queries::completion_metadata_from_input(
                                    input,
                                    &context.source_uri,
                                    context.position,
                                    context.source_generation,
                                    context.configuration_generation,
                                    &candidate_uri,
                                    candidate_index,
                                    context.format.clone(),
                                    context.resolve_documentation,
                                    context.resolve_detail,
                                    &context.records,
                                    &worker_cancellation,
                                );
                                // Keep the identity's original generation as the
                                // delivery baseline.  Dependency validation can
                                // still admit unrelated overlay changes, while
                                // a project/context switch remains stale even if
                                // this worker starts after that switch.
                                computed.source_generation = context.source_generation;
                                computed.configuration_generation =
                                    context.configuration_generation;
                                AnalysisResult {
                                    id: worker_id,
                                    source_generation: computed.source_generation,
                                    configuration_generation: computed.configuration_generation,
                                    records: computed.records,
                                    value: AnalysisResultValue::ResolveCompletion(
                                        CompletionResolutionAnalysis {
                                            token,
                                            value: computed.value,
                                        },
                                    ),
                                }
                            }
                        }
                        AnalysisRequest::DocumentSymbols { uri, hierarchical } => {
                            let computed = queries::document_symbols_from_input(
                                input,
                                &uri,
                                &worker_cancellation,
                            );
                            AnalysisResult {
                                id: worker_id,
                                source_generation: computed.source_generation,
                                configuration_generation: computed.configuration_generation,
                                records: computed.records,
                                value: AnalysisResultValue::DocumentSymbols {
                                    uri,
                                    hierarchical,
                                    value: computed.value,
                                },
                            }
                        }
                        AnalysisRequest::WorkspaceSymbols { query } => {
                            let computed = queries::workspace_symbols_from_input(
                                input,
                                &query,
                                &worker_cancellation,
                            );
                            AnalysisResult {
                                id: worker_id,
                                source_generation: computed.source_generation,
                                configuration_generation: computed.configuration_generation,
                                records: computed.records,
                                value: AnalysisResultValue::WorkspaceSymbols(computed.value),
                            }
                        }
                        AnalysisRequest::References {
                            uri,
                            position,
                            include_declaration,
                        } => {
                            let computed = queries::references_from_input(
                                input,
                                &uri,
                                position,
                                include_declaration,
                                &worker_cancellation,
                            );
                            AnalysisResult {
                                id: worker_id,
                                source_generation: computed.source_generation,
                                configuration_generation: computed.configuration_generation,
                                records: computed.records,
                                value: AnalysisResultValue::References(computed.value),
                            }
                        }
                        AnalysisRequest::DocumentHighlights { uri, position } => {
                            let computed = queries::highlights_from_input(
                                input,
                                &uri,
                                position,
                                &worker_cancellation,
                            );
                            AnalysisResult {
                                id: worker_id,
                                source_generation: computed.source_generation,
                                configuration_generation: computed.configuration_generation,
                                records: computed.records,
                                value: AnalysisResultValue::DocumentHighlights(computed.value),
                            }
                        }
                        AnalysisRequest::SelectionRanges { uri, positions } => {
                            if let Err(error) = wait_at_test_barrier(
                                TestBarrier::Selection,
                                &test_barriers,
                                &worker_cancellation,
                            ) {
                                AnalysisResult {
                                    id: worker_id,
                                    source_generation,
                                    configuration_generation,
                                    records: Vec::new(),
                                    value: AnalysisResultValue::SelectionRanges(Err(error)),
                                }
                            } else {
                                let computed = queries::selection_ranges_from_input(
                                    input,
                                    &uri,
                                    positions,
                                    &worker_cancellation,
                                );
                                AnalysisResult {
                                    id: worker_id,
                                    source_generation: computed.source_generation,
                                    configuration_generation: computed.configuration_generation,
                                    records: computed.records,
                                    value: AnalysisResultValue::SelectionRanges(computed.value),
                                }
                            }
                        }
                        AnalysisRequest::SemanticTokens { uri, range } => {
                            let computed = queries::semantic_tokens_from_input(
                                input,
                                &uri,
                                range,
                                &worker_cancellation,
                            );
                            AnalysisResult {
                                id: worker_id,
                                source_generation: computed.source_generation,
                                configuration_generation: computed.configuration_generation,
                                records: computed.records,
                                value: AnalysisResultValue::SemanticTokens(computed.value),
                            }
                        }
                        AnalysisRequest::FoldingRanges { uri } => {
                            let computed = queries::folding_ranges_from_input(
                                input,
                                &uri,
                                FoldingRangeOptions {
                                    range_limit: features.folding_range_limit,
                                    line_folding_only: features.line_folding_only,
                                    kind_value_set: features.folding_range_kind_value_set,
                                },
                                &worker_cancellation,
                            );
                            AnalysisResult {
                                id: worker_id,
                                source_generation: computed.source_generation,
                                configuration_generation: computed.configuration_generation,
                                records: computed.records,
                                value: AnalysisResultValue::FoldingRanges(computed.value),
                            }
                        }
                    }));
                let mut result = result.unwrap_or_else(|_| AnalysisResult {
                    id: panic_id,
                    source_generation,
                    configuration_generation,
                    records: Vec::new(),
                    value: panic_value,
                });
                if let Err(error) = rename::revalidate_input(
                    &validation_input,
                    &result.records,
                    &worker_cancellation,
                ) {
                    invalidate_analysis_result(&mut result, error);
                }
                let _ = sender.send(result);
            })
            .map_err(|error| format!("could not start analysis worker: {error}"))?;
        Ok(PendingAnalysis {
            cancellation,
            handle,
            client_ids: Vec::new(),
            key: None,
        })
    }

    #[cfg(test)]
    fn start(
        &mut self,
        id: RequestId,
        request: AnalysisRequest,
        workspace: &Workspace,
        features: ClientFeatures,
    ) -> Result<(), String> {
        self.enqueue_client(id, request, workspace, features, None)
    }

    fn next_computation_id(&mut self) -> Result<AnalysisComputationId, String> {
        let id = AnalysisComputationId(self.next_computation_id);
        self.next_computation_id = self
            .next_computation_id
            .checked_add(1)
            .ok_or_else(|| "analysis computation ID space exhausted".to_string())?;
        Ok(id)
    }

    fn client_recipient_count(&self) -> usize {
        let running = self
            .pending
            .values()
            .map(|job| job.client_ids.len())
            .sum::<usize>();
        let queued = self
            .queue
            .iter()
            .map(|job| match job {
                QueuedAnalysis::Client(job) => job.client_ids.len(),
                QueuedAnalysis::Diagnostic(_) => 0,
            })
            .sum::<usize>();
        running.saturating_add(queued)
    }

    fn start_diagnostics(&mut self, uri: Url, _workspace: &Workspace) -> Result<(), String> {
        if self.shutting_down || self.diagnostic_jobs.contains_key(&uri) {
            return Ok(());
        }
        if self.queue.len() >= MAX_ANALYSIS_QUEUE {
            return Err(ANALYSIS_QUEUE_FULL_MESSAGE.to_string());
        }
        let id = self.next_computation_id()?;
        self.diagnostic_jobs.insert(uri.clone(), id);
        self.queue.push(
            AnalysisPriority::Diagnostics,
            QueuedAnalysis::Diagnostic(QueuedDiagnostic { id, uri }),
        );
        Ok(())
    }

    fn enqueue_client(
        &mut self,
        id: RequestId,
        request: AnalysisRequest,
        workspace: &Workspace,
        features: ClientFeatures,
        connection: Option<&Connection>,
    ) -> Result<(), String> {
        if self.shutting_down {
            return Err("analysis server is shutting down".to_string());
        }
        if self.request_to_job.contains_key(&id) {
            return Err("analysis request ID is already in use".to_string());
        }

        let key = ObservationKey::for_request(&request, workspace);
        if let Some(key) = key.as_ref() {
            let superseded = self
                .observation_jobs
                .iter()
                .filter(|(existing, _)| existing.is_superseded_by(key))
                .map(|(_, id)| *id)
                .collect::<Vec<_>>();
            for primary_id in superseded {
                self.supersede_client(&primary_id, connection)?;
            }

            if let Some(primary_id) = self.observation_jobs.get(key).cloned() {
                if self.client_recipient_count() >= MAX_CLIENT_ANALYSIS_RECIPIENTS {
                    return Err(ANALYSIS_QUEUE_FULL_MESSAGE.to_string());
                }
                if self.attach_client(&primary_id, id.clone()) {
                    self.request_to_job.insert(id, primary_id);
                    return Ok(());
                }
                self.observation_jobs.remove(key);
            }
        }

        if self.client_recipient_count() >= MAX_CLIENT_ANALYSIS_RECIPIENTS
            || self.queue.len() >= MAX_CLIENT_ANALYSIS_QUEUE
        {
            return Err(ANALYSIS_QUEUE_FULL_MESSAGE.to_string());
        }

        let primary_id = self.next_computation_id()?;
        self.request_to_job.insert(id.clone(), primary_id);
        if let Some(key) = key.clone() {
            self.observation_jobs.insert(key, primary_id);
        }
        self.queue.push(
            AnalysisPriority::for_request(&request),
            QueuedAnalysis::Client(QueuedClientAnalysis {
                id: primary_id,
                request,
                features,
                client_ids: vec![id],
                key,
            }),
        );
        let failures = self.pump(workspace);
        self.handle_dispatch_failures(failures, connection)
    }

    fn completion_resolution_request(
        &self,
        item: &CompletionItem,
    ) -> Result<CompletionResolutionRequest, String> {
        self.completion_resolutions.request(item)
    }

    fn attach_client(&mut self, primary_id: &AnalysisComputationId, id: RequestId) -> bool {
        if let Some(job) = self.pending.get_mut(primary_id) {
            if !job.cancellation.load(std::sync::atomic::Ordering::Relaxed) {
                job.client_ids.push(id);
                return true;
            }
        }
        if let Some(QueuedAnalysis::Client(job)) = self.queue.find_mut(
            |queued| matches!(queued, QueuedAnalysis::Client(job) if &job.id == primary_id),
        ) {
            job.client_ids.push(id);
            return true;
        }
        false
    }

    fn remove_client_mapping(&mut self, id: &RequestId, primary_id: &AnalysisComputationId) {
        if self
            .request_to_job
            .get(id)
            .is_some_and(|job_id| job_id == primary_id)
        {
            self.request_to_job.remove(id);
        }
    }

    fn remove_observation(
        &mut self,
        key: Option<&ObservationKey>,
        primary_id: &AnalysisComputationId,
    ) {
        if let Some(key) = key {
            if self
                .observation_jobs
                .get(key)
                .is_some_and(|job_id| job_id == primary_id)
            {
                self.observation_jobs.remove(key);
            }
        }
    }

    fn send_client_error(
        connection: Option<&Connection>,
        ids: impl IntoIterator<Item = RequestId>,
        code: ErrorCode,
        message: &str,
    ) -> Result<(), String> {
        let Some(connection) = connection else {
            return Ok(());
        };
        for id in ids {
            send_error(connection, id, code, message)
                .map_err(|error| format!("could not send analysis response: {error}"))?;
        }
        Ok(())
    }

    fn supersede_client(
        &mut self,
        primary_id: &AnalysisComputationId,
        connection: Option<&Connection>,
    ) -> Result<(), String> {
        if let Some(QueuedAnalysis::Client(job)) = self.queue.remove_first(
            |queued| matches!(queued, QueuedAnalysis::Client(job) if &job.id == primary_id),
        ) {
            self.remove_observation(job.key.as_ref(), primary_id);
            for id in &job.client_ids {
                self.remove_client_mapping(id, primary_id);
            }
            return Self::send_client_error(
                connection,
                job.client_ids,
                ErrorCode::RequestCanceled,
                ANALYSIS_SUPERSEDED_MESSAGE,
            );
        }

        let Some(job) = self.pending.get_mut(primary_id) else {
            return Ok(());
        };
        job.cancellation
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let key = job.key.clone();
        let client_ids = std::mem::take(&mut job.client_ids);
        self.remove_observation(key.as_ref(), primary_id);
        for id in &client_ids {
            self.remove_client_mapping(id, primary_id);
        }
        Self::send_client_error(
            connection,
            client_ids,
            ErrorCode::RequestCanceled,
            ANALYSIS_SUPERSEDED_MESSAGE,
        )
    }

    fn cancel(&mut self, connection: &Connection, id: &RequestId) -> Result<(), String> {
        let Some(primary_id) = self.request_to_job.get(id).cloned() else {
            return Ok(());
        };

        let mut queued_empty = false;
        let mut found_queued = false;
        if let Some(QueuedAnalysis::Client(job)) = self.queue.find_mut(
            |queued| matches!(queued, QueuedAnalysis::Client(job) if job.id == primary_id),
        ) {
            if let Some(position) = job.client_ids.iter().position(|client_id| client_id == id) {
                job.client_ids.remove(position);
                queued_empty = job.client_ids.is_empty();
                found_queued = true;
            }
        }
        if found_queued {
            self.remove_client_mapping(id, &primary_id);
            Self::send_client_error(
                Some(connection),
                [id.clone()],
                ErrorCode::RequestCanceled,
                rename::CANCELLATION_MESSAGE,
            )?;
            if queued_empty {
                if let Some(QueuedAnalysis::Client(job)) = self.queue.remove_first(
                    |queued| matches!(queued, QueuedAnalysis::Client(job) if job.id == primary_id),
                ) {
                    self.remove_observation(job.key.as_ref(), &primary_id);
                }
            }
            return Ok(());
        }

        let mut found_running = false;
        let mut cancel_worker = false;
        let mut key = None;
        if let Some(job) = self.pending.get_mut(&primary_id) {
            if let Some(position) = job.client_ids.iter().position(|client_id| client_id == id) {
                job.client_ids.remove(position);
                cancel_worker = job.client_ids.is_empty();
                key = job.key.clone();
                found_running = true;
                if cancel_worker {
                    job.cancellation
                        .store(true, std::sync::atomic::Ordering::Relaxed);
                }
            }
        }
        if found_running {
            self.remove_client_mapping(id, &primary_id);
            if cancel_worker {
                self.remove_observation(key.as_ref(), &primary_id);
            }
            Self::send_client_error(
                Some(connection),
                [id.clone()],
                ErrorCode::RequestCanceled,
                rename::CANCELLATION_MESSAGE,
            )?;
        } else {
            self.request_to_job.remove(id);
        }
        Ok(())
    }

    fn cancel_diagnostics_for(&mut self, uris: &[Url]) {
        loop {
            let Some(QueuedAnalysis::Diagnostic(job)) = self.queue.remove_first(|queued| {
                matches!(queued, QueuedAnalysis::Diagnostic(job) if uris.contains(&job.uri))
            }) else {
                break;
            };
            if self
                .diagnostic_jobs
                .get(&job.uri)
                .is_some_and(|id| *id == job.id)
            {
                self.diagnostic_jobs.remove(&job.uri);
            }
        }
        for diagnostic in self.diagnostics.values() {
            if uris.contains(&diagnostic.uri) {
                diagnostic
                    .analysis
                    .cancellation
                    .store(true, std::sync::atomic::Ordering::Relaxed);
            }
        }
    }

    fn refresh_diagnostics(&mut self, workspace: &mut Workspace, uris: &[Url]) {
        self.cancel_diagnostics_for(uris);
        for uri in uris {
            workspace.reschedule_diagnostics(uri.clone());
        }
    }

    fn pump(&mut self, workspace: &Workspace) -> Vec<DispatchFailure> {
        if self.shutting_down {
            return Vec::new();
        }
        let mut failures = Vec::new();
        while self.pending.len().saturating_add(self.diagnostics.len()) < MAX_ANALYSIS_JOBS {
            let Some(queued) = self.queue.pop() else {
                break;
            };
            match queued {
                QueuedAnalysis::Client(job) => {
                    let primary_id = job.id;
                    self.test_barriers
                        .record_dispatch(AnalysisPriority::for_request(&job.request));
                    match self.spawn(
                        AnalysisJobId::Client(primary_id),
                        job.request,
                        workspace,
                        job.features,
                    ) {
                        Ok(mut analysis) => {
                            analysis.client_ids = job.client_ids;
                            analysis.key = job.key;
                            self.pending.insert(primary_id, analysis);
                        }
                        Err(message) => {
                            self.remove_observation(job.key.as_ref(), &primary_id);
                            for id in &job.client_ids {
                                self.remove_client_mapping(id, &primary_id);
                            }
                            failures.push(DispatchFailure {
                                client_ids: job.client_ids,
                                diagnostic: None,
                                message,
                            });
                        }
                    }
                }
                QueuedAnalysis::Diagnostic(job) => {
                    let id = job.id;
                    let uri = job.uri.clone();
                    self.test_barriers
                        .record_dispatch(AnalysisPriority::Diagnostics);
                    match self.spawn(
                        AnalysisJobId::Diagnostic(id),
                        AnalysisRequest::Diagnostics { uri: uri.clone() },
                        workspace,
                        diagnostic_features(),
                    ) {
                        Ok(analysis) => {
                            self.diagnostics
                                .insert(id, PendingDiagnostic { uri, analysis });
                        }
                        Err(message) => {
                            self.diagnostic_jobs.remove(&uri);
                            failures.push(DispatchFailure {
                                client_ids: Vec::new(),
                                diagnostic: Some(QueuedDiagnostic { id, uri }),
                                message,
                            });
                        }
                    }
                }
            }
        }
        failures
    }

    fn handle_dispatch_failures(
        &mut self,
        failures: Vec<DispatchFailure>,
        connection: Option<&Connection>,
    ) -> Result<(), String> {
        let mut first_error = None;
        for failure in failures {
            if let Some(diagnostic) = failure.diagnostic {
                if !self.shutting_down && self.queue.len() < MAX_ANALYSIS_QUEUE {
                    self.diagnostic_jobs
                        .insert(diagnostic.uri.clone(), diagnostic.id);
                    self.queue.push(
                        AnalysisPriority::Diagnostics,
                        QueuedAnalysis::Diagnostic(diagnostic),
                    );
                }
            }
            if let Some(connection) = connection {
                for id in failure.client_ids {
                    if let Err(error) =
                        send_error(connection, id, ErrorCode::RequestFailed, &failure.message)
                    {
                        first_error.get_or_insert_with(|| error.to_string());
                    }
                }
            } else if first_error.is_none() && !failure.client_ids.is_empty() {
                first_error = Some(failure.message);
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    fn poll(
        &mut self,
        connection: &Connection,
        workspace: &mut Workspace,
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        while let Ok(result) = self.receiver.try_recv() {
            match result.id {
                AnalysisJobId::Diagnostic(id) => {
                    let Some(job) = self.diagnostics.remove(&id) else {
                        continue;
                    };
                    if self
                        .diagnostic_jobs
                        .get(&job.uri)
                        .is_some_and(|job_id| *job_id == id)
                    {
                        self.diagnostic_jobs.remove(&job.uri);
                    }
                    let cancelled = job
                        .analysis
                        .cancellation
                        .load(std::sync::atomic::Ordering::Relaxed);
                    let _ = job.analysis.handle.join();
                    if cancelled {
                        workspace.reschedule_diagnostics(job.uri);
                    } else {
                        deliver_analysis_result_with_store(
                            connection,
                            workspace,
                            &mut self.completion_resolutions,
                            result,
                            None,
                        )?;
                    }
                }
                AnalysisJobId::Client(primary_id) => {
                    let Some(job) = self.pending.remove(&primary_id) else {
                        continue;
                    };
                    let cancelled = job.cancellation.load(std::sync::atomic::Ordering::Relaxed);
                    let client_ids = job.client_ids;
                    let key = job.key;
                    let _ = job.handle.join();
                    self.remove_observation(key.as_ref(), &primary_id);
                    for id in &client_ids {
                        self.remove_client_mapping(id, &primary_id);
                    }
                    if cancelled {
                        Self::send_client_error(
                            Some(connection),
                            client_ids,
                            ErrorCode::RequestCanceled,
                            rename::CANCELLATION_MESSAGE,
                        )
                        .map_err(|error| -> Box<dyn Error + Send + Sync> { error.into() })?;
                    } else if !client_ids.is_empty() {
                        for id in client_ids {
                            deliver_analysis_result_with_store(
                                connection,
                                workspace,
                                &mut self.completion_resolutions,
                                result.clone(),
                                Some(id),
                            )?;
                        }
                    }
                }
            }
        }
        let failures = self.pump(workspace);
        self.handle_dispatch_failures(failures, Some(connection))
            .map_err(|error| -> Box<dyn Error + Send + Sync> { error.into() })?;
        Ok(())
    }

    fn is_empty(&self) -> bool {
        self.pending.is_empty() && self.diagnostics.is_empty() && self.queue.is_empty()
    }

    fn shutdown(&mut self) {
        let _ = self.shutdown_inner(None);
    }

    fn shutdown_with_connection(
        &mut self,
        connection: &Connection,
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        self.shutdown_inner(Some(connection))
            .map_err(|error| -> Box<dyn Error + Send + Sync> { error.into() })
    }

    fn shutdown_inner(&mut self, connection: Option<&Connection>) -> Result<(), String> {
        if self.shutting_down {
            return Ok(());
        }
        self.shutting_down = true;
        let mut queued = std::mem::take(&mut self.queue);
        while let Some(job) = queued.pop() {
            if let QueuedAnalysis::Client(job) = job {
                Self::send_client_error(
                    connection,
                    job.client_ids.clone(),
                    ErrorCode::RequestCanceled,
                    rename::CANCELLATION_MESSAGE,
                )?;
            }
        }
        self.observation_jobs.clear();
        self.diagnostic_jobs.clear();
        self.request_to_job.clear();

        let mut pending = std::mem::take(&mut self.pending)
            .into_values()
            .collect::<Vec<_>>();
        pending.extend(
            std::mem::take(&mut self.diagnostics)
                .into_values()
                .map(|diagnostic| diagnostic.analysis),
        );
        for job in &pending {
            job.cancellation
                .store(true, std::sync::atomic::Ordering::Relaxed);
            Self::send_client_error(
                connection,
                job.client_ids.clone(),
                ErrorCode::RequestCanceled,
                rename::CANCELLATION_MESSAGE,
            )?;
        }

        let deadline = Instant::now() + ANALYSIS_SHUTDOWN_TIMEOUT;
        while !pending.is_empty() {
            let mut remaining = Vec::with_capacity(pending.len());
            for job in pending {
                if job.handle.is_finished() {
                    let _ = job.handle.join();
                } else {
                    remaining.push(job);
                }
            }
            if remaining.is_empty() {
                break;
            }
            if Instant::now() >= deadline {
                // Workers own only immutable snapshots and channel clones, so
                // dropping these handles safely detaches non-cancellable work.
                break;
            }
            pending = remaining;
            thread::sleep(ANALYSIS_POLL_INTERVAL);
        }
        Ok(())
    }
}

fn diagnostic_features() -> ClientFeatures {
    ClientFeatures {
        action_resolve: false,
        action_disabled: false,
        document_changes: false,
        hierarchical_document_symbols: false,
        hover_format: DocumentationFormat::PlainText,
        completion_format: DocumentationFormat::PlainText,
        completion_resolve_documentation: false,
        completion_resolve_detail: false,
        signature_help_format: DocumentationFormat::PlainText,
        folding_range_limit: None,
        line_folding_only: false,
        folding_range_kind_value_set: None,
    }
}

fn is_dependency_scoped_result(value: &AnalysisResultValue, records: &[SourceRecord]) -> bool {
    !records.is_empty()
        && matches!(
            value,
            AnalysisResultValue::Hover(_)
                | AnalysisResultValue::Completion(_)
                | AnalysisResultValue::ResolveCompletion(_)
                | AnalysisResultValue::SignatureHelp(_)
                | AnalysisResultValue::Navigation(_)
                | AnalysisResultValue::Formatting(_)
                | AnalysisResultValue::Diagnostics(_)
                | AnalysisResultValue::TypeDefinitions(_)
                | AnalysisResultValue::DocumentSymbols { .. }
                | AnalysisResultValue::DocumentHighlights(_)
                | AnalysisResultValue::SelectionRanges(_)
                | AnalysisResultValue::SemanticTokens(_)
                | AnalysisResultValue::FoldingRanges(_)
        )
}

#[cfg(test)]
fn deliver_analysis_result(
    connection: &Connection,
    workspace: &mut Workspace,
    result: AnalysisResult,
    client_id: Option<RequestId>,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let mut completion_resolutions = CompletionResolutionStore::new();
    deliver_analysis_result_with_store(
        connection,
        workspace,
        &mut completion_resolutions,
        result,
        client_id,
    )
}

fn deliver_analysis_result_with_store(
    connection: &Connection,
    workspace: &mut Workspace,
    completion_resolutions: &mut CompletionResolutionStore,
    result: AnalysisResult,
    client_id: Option<RequestId>,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let stale = if is_dependency_scoped_result(&result.value, &result.records) {
        workspace
            .dependency_scoped_result_is_fresh(
                result.source_generation,
                result.configuration_generation,
                &result.records,
            )
            .is_err()
    } else {
        result.source_generation != workspace.source_generation()
            || result.configuration_generation != workspace.configuration_generation()
    };
    if stale {
        if let AnalysisResultValue::Diagnostics(diagnostics) = &result.value {
            workspace.reschedule_diagnostics(diagnostics.uri.clone());
            return Ok(());
        }
        return send_error(
            connection,
            client_id
                .clone()
                .expect("non-diagnostic stale analysis result"),
            ErrorCode::RequestFailed,
            "analysis result became stale; retry the request",
        );
    }
    let mut result = result;
    if let AnalysisResultValue::Diagnostics(diagnostics) = &result.value {
        if diagnostics.discard
            || workspace.document_version(&diagnostics.uri) != diagnostics.version
        {
            workspace.reschedule_diagnostics(diagnostics.uri.clone());
            return Ok(());
        }
    }
    if let AnalysisResultValue::Navigation(navigation) = &mut result.value {
        if let Some(state) = navigation.state.take() {
            workspace.apply_navigation_state(state);
        }
    }
    match result.value {
        AnalysisResultValue::Hover(value) => match value {
            Ok(value) => send_ok(connection, client_id.clone().expect("client result"), value),
            Err(error) => {
                send_analysis_error(connection, client_id.clone().expect("client result"), error)
            }
        },
        AnalysisResultValue::Completion(analysis) => {
            let value = completion_resolutions.register(
                analysis,
                result.source_generation,
                result.configuration_generation,
                &result.records,
            );
            match value {
                Ok(value) => send_ok(
                    connection,
                    client_id.clone().expect("client result"),
                    CompletionResponse::List(value),
                ),
                Err(error) => send_analysis_error(
                    connection,
                    client_id.clone().expect("client result"),
                    error,
                ),
            }
        }
        AnalysisResultValue::ResolveCompletion(analysis) => match analysis.value {
            Ok(metadata) => match completion_resolutions.finish(&analysis.token, metadata) {
                Ok(item) => send_ok(connection, client_id.clone().expect("client result"), item),
                Err(error) => send_analysis_error(
                    connection,
                    client_id.clone().expect("client result"),
                    error,
                ),
            },
            Err(error) => {
                send_analysis_error(connection, client_id.clone().expect("client result"), error)
            }
        },
        AnalysisResultValue::SignatureHelp(value) => match value {
            Ok(value) => send_ok(connection, client_id.clone().expect("client result"), value),
            Err(error) => {
                send_analysis_error(connection, client_id.clone().expect("client result"), error)
            }
        },
        AnalysisResultValue::Navigation(navigation) => match navigation.value {
            Ok(value) => send_ok(
                connection,
                client_id.clone().expect("client result"),
                GotoDefinitionResponse::Array(value),
            ),
            Err(error) => {
                send_analysis_error(connection, client_id.clone().expect("client result"), error)
            }
        },
        AnalysisResultValue::Formatting(value) => match value {
            Ok(Some(edit)) => send_ok(
                connection,
                client_id.clone().expect("client result"),
                vec![edit],
            ),
            Ok(None) => send_ok(
                connection,
                client_id.clone().expect("client result"),
                Vec::<lsp_types::TextEdit>::new(),
            ),
            Err(error) if error == rename::CANCELLATION_MESSAGE => {
                send_analysis_error(connection, client_id.clone().expect("client result"), error)
            }
            Err(error) => send_error(
                connection,
                client_id.clone().expect("client result"),
                ErrorCode::RequestFailed,
                format!("formatting failed: {error}"),
            ),
        },
        AnalysisResultValue::Diagnostics(diagnostics) => match diagnostics.value {
            Ok(value) => send_diagnostics(connection, &diagnostics.uri, diagnostics.version, value),
            Err(error) if error == rename::CANCELLATION_MESSAGE => {
                workspace.reschedule_diagnostics(diagnostics.uri);
                Ok(())
            }
            Err(error) => send_diagnostics(
                connection,
                &diagnostics.uri,
                diagnostics.version,
                vec![crate::workspace::server_diagnostic(
                    &error,
                    lsp_types::DiagnosticSeverity::ERROR,
                )],
            ),
        },
        AnalysisResultValue::TypeDefinitions(value) => match value {
            Ok(value) => send_ok(
                connection,
                client_id.clone().expect("client result"),
                GotoDefinitionResponse::Array(value),
            ),
            Err(error) => {
                send_analysis_error(connection, client_id.clone().expect("client result"), error)
            }
        },
        AnalysisResultValue::Prepare(value) => match value {
            Ok(value) => send_ok(connection, client_id.clone().expect("client result"), value),
            Err(error) => {
                send_analysis_error(connection, client_id.clone().expect("client result"), error)
            }
        },
        AnalysisResultValue::Rename(value) => match *value {
            Ok(value) => send_ok(connection, client_id.clone().expect("client result"), value),
            Err(error) => {
                send_analysis_error(connection, client_id.clone().expect("client result"), error)
            }
        },
        AnalysisResultValue::CodeActions(value) => match value {
            Ok(value) => send_ok(connection, client_id.clone().expect("client result"), value),
            Err(error) => {
                send_analysis_error(connection, client_id.clone().expect("client result"), error)
            }
        },
        AnalysisResultValue::Resolve(value) => match *value {
            Ok(value) => send_ok(connection, client_id.clone().expect("client result"), value),
            Err(error) => {
                send_analysis_error(connection, client_id.clone().expect("client result"), error)
            }
        },
        AnalysisResultValue::DocumentSymbols {
            uri,
            hierarchical,
            value,
        } => match value {
            Ok(value) if hierarchical => {
                send_ok(connection, client_id.clone().expect("client result"), value)
            }
            Ok(value) => send_ok(
                connection,
                client_id.clone().expect("client result"),
                NavigationIndex::flatten_document_symbols(&uri, value),
            ),
            Err(error) => {
                send_analysis_error(connection, client_id.clone().expect("client result"), error)
            }
        },
        AnalysisResultValue::WorkspaceSymbols(value) => match value {
            Ok(value) => send_ok(connection, client_id.clone().expect("client result"), value),
            Err(error) => {
                send_analysis_error(connection, client_id.clone().expect("client result"), error)
            }
        },
        AnalysisResultValue::References(value) => match value {
            Ok(value) => send_ok(connection, client_id.clone().expect("client result"), value),
            Err(error) => {
                send_analysis_error(connection, client_id.clone().expect("client result"), error)
            }
        },
        AnalysisResultValue::DocumentHighlights(value) => match value {
            Ok(value) => send_ok(connection, client_id.clone().expect("client result"), value),
            Err(error) => {
                send_analysis_error(connection, client_id.clone().expect("client result"), error)
            }
        },
        AnalysisResultValue::SelectionRanges(value) => match value {
            Ok(value) => send_ok(connection, client_id.clone().expect("client result"), value),
            Err(error) => {
                send_analysis_error(connection, client_id.clone().expect("client result"), error)
            }
        },
        AnalysisResultValue::SemanticTokens(value) => match value {
            Ok(value) => send_ok(connection, client_id.clone().expect("client result"), value),
            Err(error) => {
                send_analysis_error(connection, client_id.clone().expect("client result"), error)
            }
        },
        AnalysisResultValue::FoldingRanges(value) => match value {
            Ok(value) => send_ok(connection, client_id.clone().expect("client result"), value),
            Err(error) => {
                send_analysis_error(connection, client_id.clone().expect("client result"), error)
            }
        },
    }
}

fn invalidate_analysis_result(result: &mut AnalysisResult, error: String) {
    match &mut result.value {
        AnalysisResultValue::Hover(value) => *value = Err(error),
        AnalysisResultValue::Completion(analysis) => analysis.value = Err(error),
        AnalysisResultValue::ResolveCompletion(analysis) => analysis.value = Err(error),
        AnalysisResultValue::SignatureHelp(value) => *value = Err(error),
        AnalysisResultValue::Navigation(navigation) => {
            navigation.state = None;
            navigation.value = Err(error);
        }
        AnalysisResultValue::Formatting(value) => *value = Err(error),
        AnalysisResultValue::Diagnostics(diagnostics) => {
            diagnostics.value = Err(error);
            diagnostics.discard = true;
        }
        AnalysisResultValue::TypeDefinitions(value) => *value = Err(error),
        AnalysisResultValue::Prepare(value) => *value = Err(error),
        AnalysisResultValue::Rename(value) => **value = Err(error),
        AnalysisResultValue::CodeActions(value) => *value = Err(error),
        AnalysisResultValue::Resolve(value) => **value = Err(error),
        AnalysisResultValue::DocumentSymbols { value, .. } => *value = Err(error),
        AnalysisResultValue::WorkspaceSymbols(value) => *value = Err(error),
        AnalysisResultValue::References(value) => *value = Err(error),
        AnalysisResultValue::DocumentHighlights(value) => *value = Err(error),
        AnalysisResultValue::SelectionRanges(value) => *value = Err(error),
        AnalysisResultValue::SemanticTokens(value) => *value = Err(error),
        AnalysisResultValue::FoldingRanges(value) => *value = Err(error),
    }
}

fn send_analysis_error(
    connection: &Connection,
    id: RequestId,
    error: String,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let code = if error == rename::CANCELLATION_MESSAGE {
        ErrorCode::RequestCanceled
    } else {
        ErrorCode::RequestFailed
    };
    send_error(connection, id, code, error)
}

/// Run one native LSP session over stdin/stdout.
pub fn run_stdio() -> Result<bool, Box<dyn Error + Send + Sync>> {
    run_stdio_with_config(TestBarrierConfig::disabled())
}

#[cfg(feature = "test-support")]
/// Run one native LSP session with explicitly injected deterministic test barriers.
pub fn run_stdio_with_test_barriers(
    test_barriers: TestBarrierConfig,
) -> Result<bool, Box<dyn Error + Send + Sync>> {
    run_stdio_with_config(test_barriers)
}

fn run_stdio_with_config(
    test_barriers: TestBarrierConfig,
) -> Result<bool, Box<dyn Error + Send + Sync>> {
    let (connection, io_threads) = bounded_stdio();
    let outcome = run_connection(&connection, test_barriers);
    drop(connection);
    let success = outcome?;
    io_threads.join()?;
    Ok(success)
}

struct StdioThreads {
    reader: JoinHandle<io::Result<()>>,
    writer: JoinHandle<io::Result<()>>,
}

impl StdioThreads {
    fn join(self) -> io::Result<()> {
        match self.reader.join() {
            Ok(result) => result?,
            Err(error) => std::panic::panic_any(error),
        }
        match self.writer.join() {
            Ok(result) => result,
            Err(error) => std::panic::panic_any(error),
        }
    }
}

fn bounded_stdio() -> (Connection, StdioThreads) {
    let (writer_sender, writer_receiver) = bounded::<Message>(0);
    let writer = thread::Builder::new()
        .name("PascalLspWriter".to_string())
        .spawn(move || {
            let stdout = io::stdout();
            let mut stdout = stdout.lock();
            for message in writer_receiver {
                message.write(&mut stdout)?;
            }
            Ok(())
        })
        .expect("spawn LSP writer");

    let (reader_sender, reader_receiver) = bounded::<Message>(0);
    let reader = thread::Builder::new()
        .name("PascalLspReader".to_string())
        .spawn(move || {
            let stdin = io::stdin();
            let mut stdin = BoundedReader::new(stdin.lock());
            while let Some(message) = Message::read(&mut stdin)? {
                let is_exit = matches!(
                    &message,
                    Message::Notification(notification) if notification.method == "exit"
                );
                if reader_sender.send(message).is_err() {
                    return Ok(());
                }
                if is_exit {
                    break;
                }
            }
            Ok(())
        })
        .expect("spawn LSP reader");

    (
        Connection {
            sender: writer_sender,
            receiver: reader_receiver,
        },
        StdioThreads { reader, writer },
    )
}

struct BoundedReader<R> {
    inner: R,
    header_bytes: usize,
    content_length: Option<usize>,
    payload_remaining: Option<usize>,
}

impl<R> BoundedReader<R> {
    fn new(inner: R) -> Self {
        Self {
            inner,
            header_bytes: 0,
            content_length: None,
            payload_remaining: None,
        }
    }

    fn process_header_line(&mut self, line: &str) -> io::Result<()> {
        self.header_bytes = self.header_bytes.saturating_add(line.len());
        if self.header_bytes > MAX_HEADER_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "LSP header exceeds the 8 KiB limit",
            ));
        }
        let line = line
            .strip_suffix("\r\n")
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "malformed LSP header"))?;
        if line.is_empty() {
            let content_length = self.content_length.ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "missing Content-Length header")
            })?;
            if content_length > MAX_PAYLOAD_BYTES {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "LSP payload exceeds the 8 MiB limit",
                ));
            }
            self.payload_remaining = Some(content_length);
            self.header_bytes = 0;
            self.content_length = None;
        } else if let Some((name, value)) = line.split_once(": ") {
            if name.eq_ignore_ascii_case("Content-Length") {
                self.content_length = Some(
                    value
                        .parse::<usize>()
                        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?,
                );
            }
        } else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "malformed LSP header",
            ));
        }
        Ok(())
    }
}

impl<R: BufRead> Read for BoundedReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let Some(remaining) = self.payload_remaining.as_mut() else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "LSP payload read before its header",
            ));
        };
        if *remaining == 0 || buffer.is_empty() {
            return Ok(0);
        }
        let amount = buffer.len().min(*remaining);
        let read = self.inner.read(&mut buffer[..amount])?;
        *remaining = remaining.saturating_sub(read);
        Ok(read)
    }
}

impl<R: BufRead> BufRead for BoundedReader<R> {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        self.inner.fill_buf()
    }

    fn consume(&mut self, amount: usize) {
        self.inner.consume(amount);
    }

    fn read_line(&mut self, buffer: &mut String) -> io::Result<usize> {
        if self
            .payload_remaining
            .is_some_and(|remaining| remaining > 0)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "LSP header read before payload was consumed",
            ));
        }
        if self.payload_remaining == Some(0) {
            self.payload_remaining = None;
        }
        let buffer_start = buffer.len();
        let mut total = 0;
        loop {
            let available = self.inner.fill_buf()?;
            if available.is_empty() {
                return Ok(total);
            }
            let newline = available.iter().position(|byte| *byte == b'\n');
            let amount = newline.map_or(available.len(), |index| index + 1);
            if self
                .header_bytes
                .saturating_add(total)
                .saturating_add(amount)
                > MAX_HEADER_BYTES
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "LSP header exceeds the 8 KiB limit",
                ));
            }
            let chunk = &available[..amount];
            let chunk = std::str::from_utf8(chunk)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
            buffer.push_str(chunk);
            self.inner.consume(amount);
            total = total.saturating_add(amount);
            if newline.is_some() {
                self.process_header_line(&buffer[buffer_start..])?;
                return Ok(total);
            }
        }
    }
}

fn run_connection(
    connection: &Connection,
    test_barriers: TestBarrierConfig,
) -> Result<bool, Box<dyn Error + Send + Sync>> {
    let (initialize_id, initialize, options) = loop {
        let (initialize_id, initialize_value) = connection.initialize_start()?;
        let initialize: InitializeParams = match serde_json::from_value(initialize_value) {
            Ok(initialize) => initialize,
            Err(error) => {
                send_error(
                    connection,
                    initialize_id,
                    ErrorCode::InvalidParams,
                    format!("invalid initialize parameters: {error}"),
                )?;
                continue;
            }
        };
        let options = match WorkspaceOptions::parse(initialize.initialization_options.as_ref()) {
            Ok(options) => options,
            Err(error) => {
                send_error(connection, initialize_id, ErrorCode::InvalidParams, error)?;
                continue;
            }
        };
        break (initialize_id, initialize, options);
    };
    let roots = workspace_roots(&initialize);
    let workspace_folders_supported = supports_workspace_folders(&initialize.capabilities);
    let watcher_registration_supported =
        supports_watched_file_registration(&initialize.capabilities);
    let relative_pattern_support = supports_relative_pattern(&initialize.capabilities);
    let client_features = client_features(&initialize.capabilities);
    let capabilities = server_capabilities(&initialize.capabilities);

    connection.initialize_finish(
        initialize_id,
        serde_json::json!({
            "capabilities": capabilities,
            "serverInfo": ServerInfo {
                name: SERVER_NAME.to_string(),
                version: Some(SERVER_VERSION.to_string()),
            },
        }),
    )?;

    let mut workspace = Workspace::new(roots, options);
    let watcher_registration = watcher_registration_supported
        .then(|| register_file_watcher(connection, &workspace, relative_pattern_support))
        .transpose()?;

    event_loop(
        connection,
        &mut workspace,
        workspace_folders_supported,
        client_features,
        watcher_registration,
        test_barriers,
    )
}

fn event_loop(
    connection: &Connection,
    workspace: &mut Workspace,
    workspace_folders_supported: bool,
    client_features: ClientFeatures,
    mut watcher_registration: Option<FileWatcherRegistration>,
    test_barriers: TestBarrierConfig,
) -> Result<bool, Box<dyn Error + Send + Sync>> {
    let mut shutdown_received = false;
    let mut jobs = AnalysisJobs::with_test_barriers(test_barriers);
    loop {
        if !shutdown_received {
            publish_due_diagnostics(connection, workspace, &mut jobs)?;
        }
        jobs.poll(connection, workspace)?;
        let timeout = workspace
            .next_diagnostic_timeout()
            .unwrap_or(Duration::from_secs(86_400))
            .min(if jobs.is_empty() {
                Duration::from_secs(86_400)
            } else {
                ANALYSIS_POLL_INTERVAL
            });
        let message = match connection.receiver.recv_timeout(timeout) {
            Ok(message) => message,
            Err(RecvTimeoutError::Timeout) => {
                if !shutdown_received {
                    publish_due_diagnostics(connection, workspace, &mut jobs)?;
                }
                continue;
            }
            Err(RecvTimeoutError::Disconnected) => {
                jobs.shutdown();
                return Ok(true);
            }
        };

        match message {
            Message::Request(request) if request.method == "shutdown" => {
                jobs.shutdown_with_connection(connection)?;
                send_ok(connection, request.id, ())?;
                shutdown_received = true;
            }
            Message::Request(request) => {
                if shutdown_received {
                    send_error(
                        connection,
                        request.id,
                        ErrorCode::InvalidRequest,
                        "request received after shutdown",
                    )?;
                } else {
                    let source_generation = workspace.source_generation();
                    let configuration_generation = workspace.configuration_generation();
                    handle_request(connection, workspace, request, client_features, &mut jobs)?;
                    if source_generation != workspace.source_generation()
                        || configuration_generation != workspace.configuration_generation()
                    {
                        let open_documents = workspace.open_document_uris();
                        jobs.refresh_diagnostics(workspace, &open_documents);
                    }
                    if let Some(registration) = watcher_registration.as_mut() {
                        sync_file_watcher(connection, workspace, registration)?;
                    }
                }
            }
            Message::Notification(notification) if notification.method == "exit" => {
                jobs.shutdown();
                return Ok(shutdown_received);
            }
            Message::Notification(notification) => {
                if notification.method == "$/cancelRequest" {
                    if let Ok(id) = serde_json::from_value::<RequestId>(
                        notification
                            .params
                            .get("id")
                            .cloned()
                            .unwrap_or(Value::Null),
                    ) {
                        jobs.cancel(connection, &id)
                            .map_err(|error| -> Box<dyn Error + Send + Sync> { error.into() })?;
                    }
                    continue;
                }
                match handle_notification(
                    connection,
                    workspace,
                    notification,
                    workspace_folders_supported,
                ) {
                    Ok(effect) => {
                        if let Some(registration) = watcher_registration.as_mut() {
                            sync_file_watcher(connection, workspace, registration)?;
                        }
                        jobs.cancel_diagnostics_for(&effect.cancel);
                        jobs.refresh_diagnostics(workspace, &effect.refresh);
                    }
                    Err(error) => {
                        eprintln!("pascal-lsp: notification handling failed: {error}");
                    }
                }
            }
            Message::Response(response) => {
                let watcher_response = watcher_registration
                    .as_mut()
                    .and_then(|registration| registration.handle_response(&response));
                if let Some(accepted) = watcher_response {
                    if !accepted {
                        if let Some(error) = &response.error {
                            eprintln!(
                                "pascal-lsp: watcher registration {} rejected ({}): {}",
                                response.id, error.code, error.message
                            );
                        }
                    }
                    if let Some(registration) = watcher_registration.as_mut() {
                        sync_file_watcher(connection, workspace, registration)?;
                    }
                } else if let Some(error) = response.error {
                    eprintln!(
                        "pascal-lsp: client request {} failed ({}): {}",
                        response.id, error.code, error.message
                    );
                }
            }
        }
    }
}

fn start_analysis(
    connection: &Connection,
    workspace: &Workspace,
    jobs: &mut AnalysisJobs,
    id: RequestId,
    request: AnalysisRequest,
    features: ClientFeatures,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    if let Err(error) =
        jobs.enqueue_client(id.clone(), request, workspace, features, Some(connection))
    {
        send_error(connection, id, ErrorCode::RequestFailed, error)?;
    }
    Ok(())
}

fn handle_request(
    connection: &Connection,
    workspace: &mut Workspace,
    request: Request,
    client_features: ClientFeatures,
    jobs: &mut AnalysisJobs,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    match request.method.as_str() {
        "pascal/projectContext" => {
            let id = request.id.clone();
            let params: ProjectContextRequestParams = match parse_params(&request) {
                Ok(params) => params,
                Err(error) => {
                    send_error(connection, id, ErrorCode::InvalidParams, error)?;
                    return Ok(());
                }
            };
            match workspace.project_context(&params.text_document.uri) {
                Ok(context) => send_ok(connection, id, context)?,
                Err(error) => send_error(connection, id, ErrorCode::RequestFailed, error)?,
            }
        }
        "pascal/selectProject" => {
            let id = request.id.clone();
            let params: SelectProjectRequestParams = match parse_params(&request) {
                Ok(params) => params,
                Err(error) => {
                    send_error(connection, id, ErrorCode::InvalidParams, error)?;
                    return Ok(());
                }
            };
            let project = if params.project_uri.is_null() {
                None
            } else {
                match serde_json::from_value(params.project_uri) {
                    Ok(project) => Some(project),
                    Err(error) => {
                        send_error(
                            connection,
                            id,
                            ErrorCode::InvalidParams,
                            format!("invalid projectUri: {error}"),
                        )?;
                        return Ok(());
                    }
                }
            };
            match workspace.select_project(&params.text_document.uri, project.as_ref()) {
                Ok(context) => send_ok(connection, id, context)?,
                Err(error) => send_error(connection, id, ErrorCode::RequestFailed, error)?,
            }
        }
        "textDocument/hover" => {
            let id = request.id.clone();
            let params: HoverParams = match parse_params(&request) {
                Ok(params) => params,
                Err(error) => {
                    send_error(connection, id, ErrorCode::InvalidParams, error)?;
                    return Ok(());
                }
            };
            start_analysis(
                connection,
                workspace,
                jobs,
                request.id,
                AnalysisRequest::Hover {
                    uri: canonical_file_uri(
                        &params.text_document_position_params.text_document.uri,
                    ),
                    position: params.text_document_position_params.position,
                    format: client_features.hover_format.markup_kind(),
                },
                client_features,
            )?;
        }
        "textDocument/completion" => {
            let id = request.id.clone();
            let params: CompletionParams = match parse_params(&request) {
                Ok(params) => params,
                Err(error) => {
                    send_error(connection, id, ErrorCode::InvalidParams, error)?;
                    return Ok(());
                }
            };
            start_analysis(
                connection,
                workspace,
                jobs,
                request.id,
                AnalysisRequest::Completion {
                    uri: canonical_file_uri(&params.text_document_position.text_document.uri),
                    position: params.text_document_position.position,
                    format: client_features.completion_format.markup_kind(),
                    resolve_documentation: client_features.completion_resolve_documentation,
                    resolve_detail: client_features.completion_resolve_detail,
                },
                client_features,
            )?;
        }
        "completionItem/resolve" => {
            let id = request.id.clone();
            let item: CompletionItem = match parse_params(&request) {
                Ok(item) => item,
                Err(error) => {
                    send_error(connection, id, ErrorCode::InvalidParams, error)?;
                    return Ok(());
                }
            };
            let resolve_request = match jobs.completion_resolution_request(&item) {
                Ok(resolve_request) => resolve_request,
                Err(error) => {
                    send_error(connection, id, ErrorCode::RequestFailed, error)?;
                    return Ok(());
                }
            };
            start_analysis(
                connection,
                workspace,
                jobs,
                request.id,
                AnalysisRequest::ResolveCompletion(resolve_request),
                client_features,
            )?;
        }
        "textDocument/signatureHelp" => {
            let id = request.id.clone();
            let params: SignatureHelpParams = match parse_params(&request) {
                Ok(params) => params,
                Err(error) => {
                    send_error(connection, id, ErrorCode::InvalidParams, error)?;
                    return Ok(());
                }
            };
            start_analysis(
                connection,
                workspace,
                jobs,
                request.id,
                AnalysisRequest::SignatureHelp {
                    uri: canonical_file_uri(
                        &params.text_document_position_params.text_document.uri,
                    ),
                    position: params.text_document_position_params.position,
                    format: client_features.signature_help_format.markup_kind(),
                },
                client_features,
            )?;
        }
        "textDocument/typeDefinition" => {
            let id = request.id.clone();
            let params: GotoDefinitionParams = match parse_params(&request) {
                Ok(params) => params,
                Err(error) => {
                    send_error(connection, id, ErrorCode::InvalidParams, error)?;
                    return Ok(());
                }
            };
            start_analysis(
                connection,
                workspace,
                jobs,
                request.id,
                AnalysisRequest::TypeDefinitions {
                    uri: canonical_file_uri(
                        &params.text_document_position_params.text_document.uri,
                    ),
                    position: params.text_document_position_params.position,
                },
                client_features,
            )?;
        }
        "textDocument/documentSymbol" => {
            let id = request.id.clone();
            let params: lsp_types::DocumentSymbolParams = match parse_params(&request) {
                Ok(params) => params,
                Err(error) => {
                    send_error(connection, id, ErrorCode::InvalidParams, error)?;
                    return Ok(());
                }
            };
            start_analysis(
                connection,
                workspace,
                jobs,
                request.id,
                AnalysisRequest::DocumentSymbols {
                    uri: canonical_file_uri(&params.text_document.uri),
                    hierarchical: client_features.hierarchical_document_symbols,
                },
                client_features,
            )?;
        }
        "workspace/symbol" => {
            let id = request.id.clone();
            let params: lsp_types::WorkspaceSymbolParams = match parse_params(&request) {
                Ok(params) => params,
                Err(error) => {
                    send_error(connection, id, ErrorCode::InvalidParams, error)?;
                    return Ok(());
                }
            };
            start_analysis(
                connection,
                workspace,
                jobs,
                request.id,
                AnalysisRequest::WorkspaceSymbols {
                    query: params.query,
                },
                client_features,
            )?;
        }
        "textDocument/references" => {
            let id = request.id.clone();
            let params: ReferenceParams = match parse_params(&request) {
                Ok(params) => params,
                Err(error) => {
                    send_error(connection, id, ErrorCode::InvalidParams, error)?;
                    return Ok(());
                }
            };
            start_analysis(
                connection,
                workspace,
                jobs,
                request.id,
                AnalysisRequest::References {
                    uri: canonical_file_uri(&params.text_document_position.text_document.uri),
                    position: params.text_document_position.position,
                    include_declaration: params.context.include_declaration,
                },
                client_features,
            )?;
        }
        "textDocument/documentHighlight" => {
            let id = request.id.clone();
            let params: DocumentHighlightParams = match parse_params(&request) {
                Ok(params) => params,
                Err(error) => {
                    send_error(connection, id, ErrorCode::InvalidParams, error)?;
                    return Ok(());
                }
            };
            start_analysis(
                connection,
                workspace,
                jobs,
                request.id,
                AnalysisRequest::DocumentHighlights {
                    uri: canonical_file_uri(
                        &params.text_document_position_params.text_document.uri,
                    ),
                    position: params.text_document_position_params.position,
                },
                client_features,
            )?;
        }
        "textDocument/selectionRange" => {
            let id = request.id.clone();
            let params: SelectionRangeParams = match parse_params(&request) {
                Ok(params) => params,
                Err(error) => {
                    send_error(connection, id, ErrorCode::InvalidParams, error)?;
                    return Ok(());
                }
            };
            start_analysis(
                connection,
                workspace,
                jobs,
                request.id,
                AnalysisRequest::SelectionRanges {
                    uri: canonical_file_uri(&params.text_document.uri),
                    positions: params.positions,
                },
                client_features,
            )?;
        }
        "textDocument/semanticTokens/full" => {
            let id = request.id.clone();
            let params: lsp_types::SemanticTokensParams = match parse_params(&request) {
                Ok(params) => params,
                Err(error) => {
                    send_error(connection, id, ErrorCode::InvalidParams, error)?;
                    return Ok(());
                }
            };
            start_analysis(
                connection,
                workspace,
                jobs,
                request.id,
                AnalysisRequest::SemanticTokens {
                    uri: canonical_file_uri(&params.text_document.uri),
                    range: None,
                },
                client_features,
            )?;
        }
        "textDocument/semanticTokens/range" => {
            let id = request.id.clone();
            let params: lsp_types::SemanticTokensRangeParams = match parse_params(&request) {
                Ok(params) => params,
                Err(error) => {
                    send_error(connection, id, ErrorCode::InvalidParams, error)?;
                    return Ok(());
                }
            };
            start_analysis(
                connection,
                workspace,
                jobs,
                request.id,
                AnalysisRequest::SemanticTokens {
                    uri: canonical_file_uri(&params.text_document.uri),
                    range: Some(params.range),
                },
                client_features,
            )?;
        }
        "textDocument/foldingRange" => {
            let id = request.id.clone();
            let params: FoldingRangeParams = match parse_params(&request) {
                Ok(params) => params,
                Err(error) => {
                    send_error(connection, id, ErrorCode::InvalidParams, error)?;
                    return Ok(());
                }
            };
            start_analysis(
                connection,
                workspace,
                jobs,
                request.id,
                AnalysisRequest::FoldingRanges {
                    uri: canonical_file_uri(&params.text_document.uri),
                },
                client_features,
            )?;
        }
        "textDocument/prepareRename" => {
            let id = request.id.clone();
            let params: PositionRequestParams = match parse_params(&request) {
                Ok(params) => params,
                Err(error) => {
                    send_error(connection, id, ErrorCode::InvalidParams, error)?;
                    return Ok(());
                }
            };
            start_analysis(
                connection,
                workspace,
                jobs,
                request.id,
                AnalysisRequest::Prepare {
                    uri: canonical_file_uri(&params.text_document.uri),
                    position: params.position,
                },
                client_features,
            )?;
        }
        "textDocument/rename" => {
            let id = request.id.clone();
            let params: RenameRequestParams = match parse_params(&request) {
                Ok(params) => params,
                Err(error) => {
                    send_error(connection, id, ErrorCode::InvalidParams, error)?;
                    return Ok(());
                }
            };
            start_analysis(
                connection,
                workspace,
                jobs,
                request.id,
                AnalysisRequest::Rename {
                    uri: canonical_file_uri(&params.text_document.uri),
                    position: params.position,
                    new_name: params.new_name,
                },
                client_features,
            )?;
        }
        "textDocument/codeAction" => {
            let id = request.id.clone();
            let mut params: CodeActionParams = match parse_params(&request) {
                Ok(params) => params,
                Err(error) => {
                    send_error(connection, id, ErrorCode::InvalidParams, error)?;
                    return Ok(());
                }
            };
            params.text_document.uri = canonical_file_uri(&params.text_document.uri);
            start_analysis(
                connection,
                workspace,
                jobs,
                request.id,
                AnalysisRequest::CodeActions(params),
                client_features,
            )?;
        }
        "codeAction/resolve" => {
            let id = request.id.clone();
            let action: CodeAction = match parse_params(&request) {
                Ok(action) => action,
                Err(error) => {
                    send_error(connection, id, ErrorCode::InvalidParams, error)?;
                    return Ok(());
                }
            };
            start_analysis(
                connection,
                workspace,
                jobs,
                request.id,
                AnalysisRequest::Resolve(action),
                client_features,
            )?;
        }
        "textDocument/declaration" | "textDocument/definition" | "textDocument/implementation" => {
            let id = request.id.clone();
            let params: GotoDefinitionParams = match parse_params(&request) {
                Ok(params) => params,
                Err(error) => {
                    send_error(connection, id, ErrorCode::InvalidParams, error)?;
                    return Ok(());
                }
            };
            let target = match request.method.as_str() {
                "textDocument/declaration" => NavigationTarget::Declaration,
                "textDocument/definition" => NavigationTarget::Definition,
                _ => NavigationTarget::Implementation,
            };
            start_analysis(
                connection,
                workspace,
                jobs,
                request.id,
                AnalysisRequest::Navigation {
                    uri: canonical_file_uri(
                        &params.text_document_position_params.text_document.uri,
                    ),
                    position: params.text_document_position_params.position,
                    target,
                },
                client_features,
            )?;
        }
        "textDocument/formatting" => {
            let id = request.id.clone();
            let params: DocumentFormattingParams = match parse_params(&request) {
                Ok(params) => params,
                Err(error) => {
                    send_error(connection, id, ErrorCode::InvalidParams, error)?;
                    return Ok(());
                }
            };
            start_analysis(
                connection,
                workspace,
                jobs,
                request.id,
                AnalysisRequest::Formatting {
                    uri: canonical_file_uri(&params.text_document.uri),
                },
                client_features,
            )?;
        }
        _ => send_error(
            connection,
            request.id,
            ErrorCode::MethodNotFound,
            format!("method not found: {}", request.method),
        )?,
    }
    Ok(())
}

fn handle_notification(
    connection: &Connection,
    workspace: &mut Workspace,
    notification: Notification,
    workspace_folders_supported: bool,
) -> Result<DiagnosticNotificationEffect, String> {
    match notification.method.as_str() {
        "initialized" => Ok(DiagnosticNotificationEffect::default()),
        "textDocument/didOpen" => {
            let params: DidOpenTextDocumentParams = parse_notification(&notification)?;
            let uri = params.text_document.uri.clone();
            workspace
                .open_document(
                    params.text_document.uri,
                    params.text_document.text,
                    params.text_document.version,
                )
                .map_err(|error| {
                    eprintln!("pascal-lsp: didOpen ignored: {error}");
                    error
                })?;
            let mut effect = DiagnosticNotificationEffect::default();
            effect.refresh_uri(uri);
            Ok(effect)
        }
        "textDocument/didChange" => {
            let params: DidChangeTextDocumentParams = match parse_notification(&notification) {
                Ok(params) => params,
                Err(error) => {
                    if let Some((uri, version)) = malformed_did_change_attribution(&notification) {
                        if workspace.reject_malformed_change(
                            &uri,
                            version,
                            format!("{error}; a full-document replacement is required"),
                        ) {
                            let mut effect = DiagnosticNotificationEffect::default();
                            effect.cancel_uri(uri.clone());
                            effect.refresh_uri(uri);
                            return Ok(effect);
                        }
                    }
                    return Err(error);
                }
            };
            let uri = params.text_document.uri.clone();
            workspace
                .change_document_with_changes(
                    params.text_document.uri,
                    params.content_changes,
                    params.text_document.version,
                )
                .map_err(|error| {
                    eprintln!("pascal-lsp: didChange ignored: {error}");
                    error
                })?;
            let mut effect = DiagnosticNotificationEffect::default();
            effect.refresh_uri(uri);
            Ok(effect)
        }
        "textDocument/didSave" => {
            let params: DidSaveTextDocumentParams = parse_notification(&notification)?;
            let uri = params.text_document.uri.clone();
            workspace
                .save_document(&params.text_document.uri, params.text)
                .map_err(|error| {
                    eprintln!("pascal-lsp: didSave ignored: {error}");
                    error
                })?;
            let mut effect = DiagnosticNotificationEffect::default();
            effect.refresh_uri(uri);
            Ok(effect)
        }
        "textDocument/didClose" => {
            let params: DidCloseTextDocumentParams = parse_notification(&notification)?;
            let uri = params.text_document.uri;
            if workspace.close_document(&uri) {
                send_diagnostics(connection, &uri, None, Vec::new())
                    .map_err(|error| error.to_string())?;
            }
            let mut effect = DiagnosticNotificationEffect::default();
            effect.cancel_uri(uri);
            Ok(effect)
        }
        "workspace/didChangeWatchedFiles" => {
            let params: DidChangeWatchedFilesParams = parse_notification(&notification)?;
            let mut effect = DiagnosticNotificationEffect::default();
            for change in params.changes {
                let kind = if change.typ == FileChangeType::CREATED {
                    FileChange::Created
                } else if change.typ == FileChangeType::CHANGED {
                    FileChange::Changed
                } else {
                    FileChange::Deleted
                };
                for uri in workspace.file_event(&change.uri, kind) {
                    effect.refresh_uri(uri);
                }
            }
            Ok(effect)
        }
        "workspace/didChangeWorkspaceFolders" if workspace_folders_supported => {
            let params: lsp_types::DidChangeWorkspaceFoldersParams =
                parse_notification(&notification)?;
            let added = params
                .event
                .added
                .into_iter()
                .filter_map(|folder| folder.uri.to_file_path().ok());
            let removed = params
                .event
                .removed
                .into_iter()
                .filter_map(|folder| folder.uri.to_file_path().ok());
            workspace.update_workspace_folders(added, removed);
            let mut effect = DiagnosticNotificationEffect::default();
            for uri in workspace.open_document_uris() {
                effect.refresh_uri(uri);
            }
            Ok(effect)
        }
        "workspace/didChangeWorkspaceFolders" => {
            Err("workspace/didChangeWorkspaceFolders was not advertised by this client".to_string())
        }
        _ => Err(format!(
            "unknown notification method: {}",
            notification.method
        )),
    }
}

fn publish_due_diagnostics(
    connection: &Connection,
    workspace: &mut Workspace,
    jobs: &mut AnalysisJobs,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    for (uri, version) in workspace.take_due_diagnostic_requests() {
        if let Err(error) = jobs.start_diagnostics(uri.clone(), workspace) {
            if error == ANALYSIS_QUEUE_FULL_MESSAGE {
                workspace.retry_diagnostics(uri);
            } else {
                send_diagnostics(
                    connection,
                    &uri,
                    version,
                    vec![crate::workspace::server_diagnostic(
                        &error,
                        lsp_types::DiagnosticSeverity::ERROR,
                    )],
                )?;
            }
        }
    }
    Ok(())
}

fn send_diagnostics(
    connection: &Connection,
    uri: &Url,
    version: Option<i32>,
    diagnostics: Vec<lsp_types::Diagnostic>,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    connection
        .sender
        .send(Message::Notification(Notification::new(
            "textDocument/publishDiagnostics".to_string(),
            PublishDiagnosticsParams {
                uri: uri.clone(),
                diagnostics,
                version,
            },
        )))?;
    Ok(())
}

fn register_file_watcher(
    connection: &Connection,
    workspace: &Workspace,
    relative_pattern_support: bool,
) -> Result<FileWatcherRegistration, Box<dyn Error + Send + Sync>> {
    let mut registration =
        FileWatcherRegistration::with_relative_pattern_support(relative_pattern_support);
    let mut watchers = vec![FileSystemWatcher {
        glob_pattern: GlobPattern::String("**/*.{pas,dpr,dpk,dproj,optset,toml}".to_string()),
        kind: Some(WatchKind::Create | WatchKind::Change | WatchKind::Delete),
    }];
    let paths = registration.new_paths(workspace.configuration_watch_paths());
    for path in &paths {
        watchers.push(configuration_watcher(path));
    }
    let request_id = RequestId::from("pascal-lsp-register-watcher".to_string());
    registration.mark_pending(request_id.clone(), &paths);
    send_file_watcher_registration(
        connection,
        "pascal-lsp-file-watcher",
        "pascal-lsp-register-watcher",
        watchers,
    )
    .inspect_err(|_| {
        registration.complete(&request_id, true);
    })?;
    Ok(registration)
}

fn sync_file_watcher(
    connection: &Connection,
    workspace: &Workspace,
    registration: &mut FileWatcherRegistration,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let mut watchers = Vec::new();
    let paths = registration.new_paths(workspace.configuration_watch_paths());
    for path in &paths {
        watchers.push(configuration_watcher(path));
    }
    if paths.is_empty() {
        return Ok(());
    }
    let id = registration.next_id;
    registration.next_id = registration.next_id.saturating_add(1);
    let request_id = format!("pascal-lsp-register-watcher-{id}");
    let request_id_value = RequestId::from(request_id.clone());
    registration.mark_pending(request_id_value.clone(), &paths);
    send_file_watcher_registration(
        connection,
        &format!("pascal-lsp-file-watcher-{id}"),
        &request_id,
        watchers,
    )
    .inspect_err(|_| {
        registration.complete(&request_id_value, true);
    })
}

fn configuration_watcher(path: &Path) -> FileSystemWatcher {
    let base_uri = Url::from_file_path(path.parent().unwrap_or(path))
        .expect("configuration watcher base must be a file URI");
    let pattern = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "*".to_string());
    FileSystemWatcher {
        glob_pattern: GlobPattern::Relative(RelativePattern {
            base_uri: OneOf::Right(base_uri),
            pattern,
        }),
        kind: Some(WatchKind::Create | WatchKind::Change | WatchKind::Delete),
    }
}

fn send_file_watcher_registration(
    connection: &Connection,
    registration_id: &str,
    request_id: &str,
    watchers: Vec<FileSystemWatcher>,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let registration = Registration {
        id: registration_id.to_string(),
        method: "workspace/didChangeWatchedFiles".to_string(),
        register_options: Some(serde_json::to_value(
            lsp_types::DidChangeWatchedFilesRegistrationOptions { watchers },
        )?),
    };
    let request = Request::new(
        RequestId::from(request_id.to_string()),
        "client/registerCapability".to_string(),
        RegistrationParams {
            registrations: vec![registration],
        },
    );
    connection.sender.send(Message::Request(request))?;
    Ok(())
}

fn watcher_path_key(path: &Path) -> String {
    let path = path.to_string_lossy().replace('\\', "/");
    #[cfg(windows)]
    {
        path.to_ascii_lowercase()
    }
    #[cfg(not(windows))]
    {
        path
    }
}

fn parse_params<T: DeserializeOwned>(request: &Request) -> Result<T, String> {
    serde_json::from_value(request.params.clone())
        .map_err(|error| format!("invalid parameters for {}: {error}", request.method))
}

fn parse_notification<T: DeserializeOwned>(notification: &Notification) -> Result<T, String> {
    serde_json::from_value(notification.params.clone())
        .map_err(|error| format!("invalid parameters for {}: {error}", notification.method))
}

fn malformed_did_change_attribution(notification: &Notification) -> Option<(Url, i32)> {
    let text_document = notification.params.get("textDocument")?.as_object()?;
    let uri = Url::parse(text_document.get("uri")?.as_str()?).ok()?;
    let version = i32::try_from(text_document.get("version")?.as_i64()?).ok()?;
    Some((uri, version))
}

fn send_ok<T: serde::Serialize>(
    connection: &Connection,
    id: RequestId,
    value: T,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    connection
        .sender
        .send(Message::Response(Response::new_ok(id, value)))?;
    Ok(())
}

fn send_error(
    connection: &Connection,
    id: RequestId,
    code: ErrorCode,
    message: impl Into<String>,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    connection.sender.send(Message::Response(Response::new_err(
        id,
        code as i32,
        message.into(),
    )))?;
    Ok(())
}

fn server_capabilities(client: &ClientCapabilities) -> Value {
    let mut capabilities = serde_json::json!({
        "positionEncoding": "utf-16",
        "textDocumentSync": {
            "openClose": true,
            "change": 2,
            "save": true
        },
        "hoverProvider": true,
        "completionProvider": {"triggerCharacters": ["."], "resolveProvider": true},
        "signatureHelpProvider": {"triggerCharacters": ["(", ","]},
        "typeDefinitionProvider": true,
        "declarationProvider": true,
        "definitionProvider": true,
        "implementationProvider": true,
        "documentSymbolProvider": true,
        "workspaceSymbolProvider": true,
        "referencesProvider": true,
        "documentHighlightProvider": true,
        "selectionRangeProvider": true,
        "foldingRangeProvider": true,
        "semanticTokensProvider": {
            "legend": crate::NavigationIndex::semantic_tokens_legend(),
            "range": true,
            "full": true
        },
        "documentFormattingProvider": true,
        "renameProvider": {"prepareProvider": true},
        "codeActionProvider": {
            "codeActionKinds": ["quickfix"],
            "resolveProvider": true
        },
        "experimental": {
            "projectSelection": true
        }
    });
    if supports_workspace_folders(client) {
        capabilities["workspace"] = serde_json::json!({
            "workspaceFolders": {
                "supported": true,
                "changeNotifications": true
            }
        });
    }
    capabilities
}

fn client_features(client: &ClientCapabilities) -> ClientFeatures {
    let value = serde_json::to_value(client).unwrap_or(Value::Null);
    let code_action = &value["textDocument"]["codeAction"];
    let action_resolve = code_action["dataSupport"].as_bool().unwrap_or(false)
        && code_action["resolveSupport"]["properties"]
            .as_array()
            .is_some_and(|properties| {
                properties
                    .iter()
                    .any(|property| property.as_str() == Some("edit"))
            });
    let action_disabled = code_action["disabledSupport"].as_bool().unwrap_or(false);
    let document_changes = value["workspace"]["workspaceEdit"]["documentChanges"]
        .as_bool()
        .unwrap_or(false);
    let hierarchical_document_symbols =
        value["textDocument"]["documentSymbol"]["hierarchicalDocumentSymbolSupport"]
            .as_bool()
            .unwrap_or(false);
    let hover_format =
        preferred_documentation_format(&value, &["textDocument", "hover", "contentFormat"]);
    let completion_format = preferred_documentation_format(
        &value,
        &[
            "textDocument",
            "completion",
            "completionItem",
            "documentationFormat",
        ],
    );
    let completion_resolve_properties =
        &value["textDocument"]["completion"]["completionItem"]["resolveSupport"]["properties"];
    let completion_resolve_documentation =
        completion_resolve_properties
            .as_array()
            .is_some_and(|properties| {
                properties
                    .iter()
                    .any(|property| property.as_str() == Some("documentation"))
            });
    let completion_resolve_detail =
        completion_resolve_properties
            .as_array()
            .is_some_and(|properties| {
                properties
                    .iter()
                    .any(|property| property.as_str() == Some("detail"))
            });
    let signature_help_format = preferred_documentation_format(
        &value,
        &[
            "textDocument",
            "signatureHelp",
            "signatureInformation",
            "documentationFormat",
        ],
    );
    let folding = &value["textDocument"]["foldingRange"];
    let folding_range_limit = folding["rangeLimit"]
        .as_u64()
        .map(|limit| usize::try_from(limit).unwrap_or(usize::MAX));
    let line_folding_only = folding["lineFoldingOnly"].as_bool().unwrap_or(false);
    let folding_range_kind_value_set =
        folding["foldingRangeKind"]["valueSet"]
            .as_array()
            .map(|kinds| {
                kinds.iter().fold(0, |mask, kind| {
                    mask | match kind.as_str() {
                        Some("comment") => FOLDING_KIND_COMMENT,
                        Some("imports") => FOLDING_KIND_IMPORTS,
                        Some("region") => FOLDING_KIND_REGION,
                        _ => 0,
                    }
                })
            });
    ClientFeatures {
        action_resolve,
        action_disabled,
        document_changes,
        hierarchical_document_symbols,
        hover_format,
        completion_format,
        completion_resolve_documentation,
        completion_resolve_detail,
        signature_help_format,
        folding_range_limit,
        line_folding_only,
        folding_range_kind_value_set,
    }
}

fn preferred_documentation_format(value: &Value, path: &[&str]) -> DocumentationFormat {
    let mut current = value;
    for key in path {
        current = &current[*key];
    }
    let Some(formats) = current.as_array() else {
        return DocumentationFormat::PlainText;
    };
    for format in formats {
        match format.as_str() {
            Some("plaintext") => return DocumentationFormat::PlainText,
            Some("markdown") => return DocumentationFormat::Markdown,
            _ => {}
        }
    }
    DocumentationFormat::PlainText
}

fn supports_workspace_folders(client: &ClientCapabilities) -> bool {
    client
        .workspace
        .as_ref()
        .and_then(|workspace| workspace.workspace_folders)
        .unwrap_or(false)
}

fn supports_watched_file_registration(client: &ClientCapabilities) -> bool {
    client
        .workspace
        .as_ref()
        .and_then(|workspace| workspace.did_change_watched_files)
        .and_then(|watched| watched.dynamic_registration)
        .unwrap_or(false)
}

fn supports_relative_pattern(client: &ClientCapabilities) -> bool {
    client
        .workspace
        .as_ref()
        .and_then(|workspace| workspace.did_change_watched_files)
        .and_then(|watched| watched.relative_pattern_support)
        .unwrap_or(false)
}

#[allow(deprecated)]
fn workspace_roots(initialize: &InitializeParams) -> Vec<PathBuf> {
    if let Some(folders) = &initialize.workspace_folders {
        if !folders.is_empty() {
            let paths: Vec<PathBuf> = folders
                .iter()
                .filter_map(|folder: &WorkspaceFolder| folder.uri.to_file_path().ok())
                .collect();
            if !paths.is_empty() {
                return paths;
            }
        }
    }
    if let Some(uri) = &initialize.root_uri {
        if let Ok(path) = uri.to_file_path() {
            return vec![path];
        }
    }
    if let Some(path) = &initialize.root_path {
        return vec![PathBuf::from(path)];
    }
    vec![std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))]
}

#[cfg(test)]
mod tests {
    use super::{
        ANALYSIS_QUEUE_FULL_MESSAGE, ANALYSIS_SUPERSEDED_MESSAGE, AnalysisComputationId,
        AnalysisJobId, AnalysisJobs, AnalysisPriority, AnalysisRequest, AnalysisResult,
        AnalysisResultValue, BoundedReader, ClientFeatures, CompletionAnalysis,
        CompletionResolutionSeed, CompletionResolutionStore, CompletionResult, DocumentationFormat,
        FileWatcherRegistration, MAX_ANALYSIS_QUEUE, MAX_COMPLETION_RESOLUTION_CONTEXT_BYTES,
        MAX_COMPLETION_RESOLUTION_DATA_BYTES, MAX_COMPLETION_RESOLUTION_RECORDS,
        MAX_CONFIGURATION_WATCH_PATHS, MAX_PAYLOAD_BYTES, MAX_WATCHER_REGISTRATION_RETRIES,
        PendingAnalysis, PriorityQueue, deliver_analysis_result, invalidate_analysis_result,
    };
    use crate::workspace::Workspace;
    use crate::workspace::rename::{SourceRecord, install_snapshot_priority_barrier};
    use crossbeam_channel::RecvTimeoutError;
    use lsp_server::{Connection, Message, RequestId, Response};
    use lsp_types::{
        ClientCapabilities, CompletionItem, CompletionList, MarkupKind, Position,
        PrepareRenameResponse, Range, Url,
    };
    use pascal_project::delphi_overrides::OverrideSession;
    use std::fs;
    use std::io::{Cursor, ErrorKind};
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    use std::sync::mpsc;
    use std::thread;
    use std::time::{Duration, Instant};

    fn test_workspace(
        roots: Vec<PathBuf>,
        options: crate::workspace::WorkspaceOptions,
    ) -> Workspace {
        Workspace::with_override_session(roots, options, OverrideSession::new(None))
    }

    fn symbol_client_features() -> ClientFeatures {
        ClientFeatures {
            action_resolve: false,
            action_disabled: false,
            document_changes: false,
            hierarchical_document_symbols: false,
            hover_format: DocumentationFormat::PlainText,
            completion_format: DocumentationFormat::PlainText,
            completion_resolve_documentation: false,
            completion_resolve_detail: false,
            signature_help_format: DocumentationFormat::PlainText,
            folding_range_limit: None,
            line_folding_only: false,
            folding_range_kind_value_set: None,
        }
    }

    #[test]
    fn documentation_formats_are_negotiated_independently() {
        let capabilities: ClientCapabilities = serde_json::from_value(serde_json::json!({
            "textDocument": {
                "hover": {"contentFormat": ["plaintext", "markdown"]},
                "completion": {
                    "completionItem": {"documentationFormat": ["markdown"]}
                },
                "signatureHelp": {
                    "signatureInformation": {
                        "documentationFormat": ["markdown", "plaintext"]
                    }
                }
            }
        }))
        .expect("documentation capabilities");

        let features = super::client_features(&capabilities);
        assert_eq!(features.hover_format, DocumentationFormat::PlainText);
        assert_eq!(features.completion_format, DocumentationFormat::Markdown);
        assert_eq!(
            features.signature_help_format,
            DocumentationFormat::Markdown
        );
    }

    fn test_completion_analysis(uri: &Url, index: usize) -> CompletionAnalysis {
        CompletionAnalysis {
            uri: uri.clone(),
            position: Position::new(0, 0),
            format: MarkupKind::PlainText,
            resolve_documentation: true,
            resolve_detail: true,
            value: Ok(CompletionResult {
                list: CompletionList {
                    is_incomplete: false,
                    items: vec![CompletionItem {
                        label: format!("Candidate{index}"),
                        ..CompletionItem::default()
                    }],
                },
                seeds: vec![CompletionResolutionSeed::test_new(uri.clone(), index)],
            }),
        }
    }

    #[test]
    fn completion_resolution_store_rejects_tampering_and_foreign_entries() {
        let uri = Url::parse("file:///completion-store.pas").expect("completion URI");
        let mut owner = CompletionResolutionStore::new();
        let item = owner
            .register(test_completion_analysis(&uri, 0), 1, 1, &[])
            .expect("store registration")
            .items
            .into_iter()
            .next()
            .expect("stored item");

        let mut tampered = item.clone();
        tampered.data = Some(serde_json::json!({
            "version": 1,
            "token": "not-the-issued-token",
            "proof": "not-the-issued-proof"
        }));
        let error = owner
            .request(&tampered)
            .expect_err("tampered resolve data must be rejected");
        assert!(error.contains("stale") || error.contains("tampered"));

        let foreign = CompletionResolutionStore::new();
        let error = foreign
            .request(&item)
            .expect_err("foreign resolve data must be rejected");
        assert!(error.contains("foreign") || error.contains("evicted"));

        let oversized = CompletionItem {
            data: Some(serde_json::json!({
                "version": 1,
                "token": "x".repeat(MAX_COMPLETION_RESOLUTION_DATA_BYTES),
                "proof": "x"
            })),
            ..CompletionItem::default()
        };
        let error = owner
            .request(&oversized)
            .expect_err("oversized resolve data must be rejected");
        assert!(error.contains("bounded size"));
    }

    #[test]
    fn completion_resolution_store_evicts_the_oldest_entry() {
        let uri = Url::parse("file:///completion-store-eviction.pas").expect("completion URI");
        let mut store = CompletionResolutionStore::new();
        let mut first = None;
        let mut last = None;
        for index in 0..=super::MAX_COMPLETION_RESOLUTION_ENTRIES {
            let output = store
                .register(test_completion_analysis(&uri, index), 1, 1, &[])
                .expect("bounded store registration");
            if index == 0 {
                first = output.items.first().cloned();
            }
            last = output.items.first().cloned();
        }
        let first = first.expect("first stored item");
        let last = last.expect("last stored item");
        assert!(
            store.request(&first).is_err(),
            "oldest completion entry must be evicted"
        );
        assert!(
            store.request(&last).is_ok(),
            "newest entry must remain usable"
        );
    }

    fn test_completion_record(uri: Url) -> SourceRecord {
        SourceRecord {
            uri,
            text: String::new(),
            version: Some(1),
            stamp: None,
            open: true,
            path: None,
            path_stamp: None,
            content_hash: None,
            parsed_text_hash: None,
            content_bytes: None,
            candidate_membership: None,
            read_policy: None,
            path_entry: None,
            include_payload: false,
            missing_provider_candidate: false,
            missing_provider_scope: None,
            auto_import_provider_observation: false,
            auto_import_scopes: Vec::new(),
        }
    }

    #[test]
    fn completion_resolution_store_enforces_context_record_and_byte_bounds() {
        let base_uri = Url::parse("file:///completion-store-bounds.pas").expect("completion URI");
        let at_limit = (0..MAX_COMPLETION_RESOLUTION_RECORDS)
            .map(|index| {
                test_completion_record(
                    Url::parse(&format!("file:///completion-record-{index}.pas"))
                        .expect("record URI"),
                )
            })
            .collect::<Vec<_>>();
        let mut records_store = CompletionResolutionStore::new();
        records_store
            .register(test_completion_analysis(&base_uri, 0), 1, 1, &at_limit)
            .expect("the exact retained-record limit must be accepted");

        let too_many = (0..=MAX_COMPLETION_RESOLUTION_RECORDS)
            .map(|index| {
                test_completion_record(
                    Url::parse(&format!("file:///completion-too-many-{index}.pas"))
                        .expect("record URI"),
                )
            })
            .collect::<Vec<_>>();
        let mut count_store = CompletionResolutionStore::new();
        let error = count_store
            .register(test_completion_analysis(&base_uri, 0), 1, 1, &too_many)
            .expect_err("the retained-record limit must be enforced");
        assert!(
            error.contains("observations") || error.contains("bounded"),
            "{error}"
        );

        let huge_uri = Url::parse(&format!(
            "file:///{}",
            "x".repeat(MAX_COMPLETION_RESOLUTION_CONTEXT_BYTES)
        ))
        .expect("large completion URI");
        let mut byte_store = CompletionResolutionStore::new();
        let error = byte_store
            .register(
                test_completion_analysis(&huge_uri, 0),
                1,
                1,
                &[test_completion_record(huge_uri)],
            )
            .expect_err("the retained context byte limit must be enforced");
        assert!(
            error.contains("byte") || error.contains("bounded"),
            "{error}"
        );
    }

    fn receive_analysis_result(jobs: &mut AnalysisJobs, id: &RequestId) -> AnalysisResult {
        let result = jobs.receiver.recv().expect("analysis worker result");
        let computation_id = *jobs
            .request_to_job
            .get(id)
            .expect("request-to-computation mapping");
        let pending = jobs
            .pending
            .remove(&computation_id)
            .expect("pending analysis job");
        assert!(
            pending.handle.join().is_ok(),
            "analysis worker must exit cleanly"
        );
        assert_eq!(result.id, AnalysisJobId::Client(computation_id));
        result
    }

    fn deliver_successfully(workspace: &mut Workspace, id: RequestId, result: AnalysisResult) {
        let (server, client) = Connection::memory();
        deliver_analysis_result(&server, workspace, result, Some(id.clone()))
            .expect("deliver control result");
        let Message::Response(response) = client.receiver.recv().expect("control response") else {
            panic!("expected a control response");
        };
        assert_eq!(response.id, id);
        assert!(
            response.error.is_none(),
            "unchanged computed result must deliver: {response:?}"
        );
        assert!(
            response.result.is_some(),
            "successful result must be delivered"
        );
    }

    fn assert_workspace_symbols_were_computed(result: &AnalysisResult) {
        match &result.value {
            AnalysisResultValue::WorkspaceSymbols(Ok(symbols)) => {
                assert!(
                    !symbols.is_empty(),
                    "the computed symbol result must not be empty"
                );
            }
            AnalysisResultValue::WorkspaceSymbols(Err(error)) => {
                panic!("symbol worker failed before delivery: {error}");
            }
            _ => panic!("expected a workspace-symbol result"),
        }
        assert!(
            !result.records.is_empty(),
            "the computed symbol result must carry its source read set"
        );
    }

    fn assert_hover_was_computed(result: &AnalysisResult) {
        match &result.value {
            AnalysisResultValue::Hover(Ok(Some(hover))) => {
                assert!(
                    hover.range.is_some(),
                    "hover result must carry its identifier range"
                );
            }
            AnalysisResultValue::Hover(Ok(None)) => panic!("hover worker returned no result"),
            AnalysisResultValue::Hover(Err(error)) => {
                panic!("hover worker failed before delivery: {error}")
            }
            _ => panic!("expected a hover result"),
        }
        assert!(
            !result.records.is_empty(),
            "the computed hover result must carry its source read set"
        );
    }

    fn assert_completion_was_computed(result: &AnalysisResult) {
        match &result.value {
            AnalysisResultValue::Completion(CompletionAnalysis {
                value: Ok(completion),
                ..
            }) => {
                assert!(
                    !completion.list.items.is_empty(),
                    "the computed completion result must not be empty"
                );
            }
            AnalysisResultValue::Completion(CompletionAnalysis {
                value: Err(error), ..
            }) => {
                panic!("completion worker failed before delivery: {error}");
            }
            _ => panic!("expected a completion result"),
        }
        assert!(
            !result.records.is_empty(),
            "the computed completion result must carry its source read set"
        );
    }

    fn assert_signature_help_was_computed(result: &AnalysisResult) {
        match &result.value {
            AnalysisResultValue::SignatureHelp(Ok(Some(signature_help))) => {
                assert!(
                    !signature_help.signatures.is_empty(),
                    "the computed signature-help result must not be empty"
                );
            }
            AnalysisResultValue::SignatureHelp(Ok(None)) => {
                panic!("signature-help worker returned no result")
            }
            AnalysisResultValue::SignatureHelp(Err(error)) => {
                panic!("signature-help worker failed before delivery: {error}");
            }
            _ => panic!("expected a signature-help result"),
        }
        assert!(
            !result.records.is_empty(),
            "the computed signature-help result must carry its source read set"
        );
    }

    fn assert_semantic_tokens_were_computed(result: &AnalysisResult) {
        match &result.value {
            AnalysisResultValue::SemanticTokens(Ok(tokens)) => {
                assert!(
                    !tokens.data.is_empty(),
                    "the computed semantic-token result must not be empty"
                );
            }
            AnalysisResultValue::SemanticTokens(Err(error)) => {
                panic!("semantic-token worker failed before delivery: {error}");
            }
            _ => panic!("expected a semantic-token result"),
        }
        assert!(
            !result.records.is_empty(),
            "the computed semantic-token result must carry its source read set"
        );
    }

    fn assert_folding_ranges_were_computed(result: &AnalysisResult) {
        match &result.value {
            AnalysisResultValue::FoldingRanges(Ok(ranges)) => {
                assert!(
                    !ranges.is_empty(),
                    "the computed folding-range result must not be empty"
                );
            }
            AnalysisResultValue::FoldingRanges(Err(error)) => {
                panic!("folding-range worker failed before delivery: {error}");
            }
            _ => panic!("expected a folding-range result"),
        }
        assert!(
            !result.records.is_empty(),
            "the computed folding-range result must carry its source read set"
        );
    }

    struct AssistanceDeliveryFixture {
        _temp: tempfile::TempDir,
        workspace: Workspace,
        main_uri: Url,
        provider_uri: Url,
        project_b_uri: Url,
        provider_source: String,
    }

    #[derive(Clone, Copy)]
    enum AssistanceRequestKind {
        Completion,
        SignatureHelp,
    }

    fn assistance_delivery_fixture() -> AssistanceDeliveryFixture {
        let temp = tempfile::tempdir().expect("temporary workspace");
        let root = temp.path().to_path_buf();
        let main = root.join("Main.pas");
        let provider = root.join("Provider.pas");
        let project_a = root.join("A.dproj");
        let project_b = root.join("B.dproj");
        let main_source = "unit Main;\ninterface\nuses Provider;\nimplementation\nprocedure Caller;\nbegin\n  Provided(1);\nend;\nend.\n";
        let provider_source = "unit Provider;\ninterface\nprocedure Provided(Value: Integer);\nimplementation\nprocedure Provided(Value: Integer);\nbegin\nend;\nend.\n".to_string();
        let project_source = "<Project><PropertyGroup><MainSource>Main.pas</MainSource></PropertyGroup><ItemGroup><DCCReference Include=\"Provider.pas\" /></ItemGroup></Project>";
        fs::create_dir_all(&root).expect("fixture directory");
        fs::write(&main, main_source).expect("main source");
        fs::write(&provider, &provider_source).expect("provider source");
        fs::write(&project_a, project_source).expect("project A");
        fs::write(&project_b, project_source).expect("project B");

        let main_uri = Url::from_file_path(&main).expect("main URI");
        let provider_uri = Url::from_file_path(&provider).expect("provider URI");
        let project_a_uri = Url::from_file_path(&project_a).expect("project A URI");
        let project_b_uri = Url::from_file_path(&project_b).expect("project B URI");
        let mut workspace = Workspace::new(vec![root], Default::default());
        workspace
            .open_document(provider_uri.clone(), provider_source.clone(), 1)
            .expect("provider overlay");
        workspace
            .select_project(&main_uri, Some(&project_a_uri))
            .expect("select project A");

        AssistanceDeliveryFixture {
            _temp: temp,
            workspace,
            main_uri,
            provider_uri,
            project_b_uri,
            provider_source,
        }
    }

    fn assistance_request(
        kind: AssistanceRequestKind,
        main_uri: Url,
        position: Position,
    ) -> AnalysisRequest {
        match kind {
            AssistanceRequestKind::Completion => AnalysisRequest::Completion {
                uri: main_uri,
                position,
                format: MarkupKind::Markdown,
                resolve_documentation: false,
                resolve_detail: false,
            },
            AssistanceRequestKind::SignatureHelp => AnalysisRequest::SignatureHelp {
                uri: main_uri,
                position,
                format: MarkupKind::Markdown,
            },
        }
    }

    fn assistance_position(source: &str, needle: &str) -> Position {
        let offset = source
            .find(needle)
            .unwrap_or_else(|| panic!("{needle:?} not found in assistance fixture"))
            .saturating_add(needle.len());
        crate::text::offset_to_position(source, offset).expect("assistance position")
    }

    fn assert_stale_delivery(workspace: &mut Workspace, id: RequestId, result: AnalysisResult) {
        let (server, client) = Connection::memory();
        deliver_analysis_result(&server, workspace, result, Some(id.clone()))
            .expect("deliver stale result");
        let Message::Response(response) = client.receiver.recv().expect("stale response") else {
            panic!("expected a stale response");
        };
        assert_eq!(response.id, id);
        assert_eq!(response.error.expect("stale result error").code, -32803);
    }

    fn start_assistance_for_delivery(
        fixture: &AssistanceDeliveryFixture,
        kind: AssistanceRequestKind,
        id: &str,
    ) -> AnalysisResult {
        let main_source = "unit Main;\ninterface\nuses Provider;\nimplementation\nprocedure Caller;\nbegin\n  Provided(1);\nend;\nend.\n";
        let position = match kind {
            AssistanceRequestKind::Completion => assistance_position(main_source, "  Pro"),
            AssistanceRequestKind::SignatureHelp => assistance_position(main_source, "Provided("),
        };
        let request_id = RequestId::from(id.to_string());
        let mut jobs = AnalysisJobs::new();
        jobs.start(
            request_id.clone(),
            assistance_request(kind, fixture.main_uri.clone(), position),
            &fixture.workspace,
            symbol_client_features(),
        )
        .expect("start assistance request");
        receive_analysis_result(&mut jobs, &request_id)
    }

    fn assert_type_definitions_were_computed(result: &AnalysisResult) {
        match &result.value {
            AnalysisResultValue::TypeDefinitions(Ok(locations)) => {
                assert!(
                    !locations.is_empty(),
                    "the computed type-definition result must not be empty"
                );
            }
            AnalysisResultValue::TypeDefinitions(Err(error)) => {
                panic!("type-definition worker failed before delivery: {error}");
            }
            _ => panic!("expected a type-definition result"),
        }
        assert!(
            !result.records.is_empty(),
            "the computed type-definition result must carry its source read set"
        );
    }

    #[test]
    fn priority_queue_is_fifo_within_priority_and_bounds_interactive_bursts() {
        let mut queue = PriorityQueue::new();
        queue.push(AnalysisPriority::Bulk, "bulk-1");
        queue.push(AnalysisPriority::Interactive, "interactive-1");
        queue.push(AnalysisPriority::Interactive, "interactive-2");
        queue.push(AnalysisPriority::Interactive, "interactive-3");
        queue.push(AnalysisPriority::Interactive, "interactive-4");
        queue.push(AnalysisPriority::Diagnostics, "diagnostic-1");
        queue.push(AnalysisPriority::Bulk, "bulk-2");

        let order = (0..7)
            .map(|_| queue.pop().expect("queued item"))
            .collect::<Vec<_>>();
        assert_eq!(
            order,
            vec![
                "interactive-1",
                "interactive-2",
                "interactive-3",
                "diagnostic-1",
                "interactive-4",
                "bulk-1",
                "bulk-2",
            ]
        );
        assert_eq!(queue.len(), 0);
        assert!(queue.is_empty());
        let mut removable = PriorityQueue::new();
        removable.push(AnalysisPriority::Bulk, "removed");
        assert_eq!(
            removable.remove_first(|item| *item == "removed"),
            Some("removed")
        );
        assert_eq!(MAX_ANALYSIS_QUEUE, 32);
        assert_eq!(
            ANALYSIS_QUEUE_FULL_MESSAGE,
            "analysis queue is full; retry the request"
        );
        assert_eq!(
            ANALYSIS_SUPERSEDED_MESSAGE,
            "request superseded by a newer document version"
        );
    }

    #[test]
    fn bounded_reader_rejects_oversized_content_length_before_payload_read() {
        let header = format!(
            "Content-Length: {}\r\n\r\n",
            MAX_PAYLOAD_BYTES.saturating_add(1)
        );
        let mut reader = BoundedReader::new(Cursor::new(header.into_bytes()));
        let error = Message::read(&mut reader).expect_err("oversized frame must be rejected");
        assert_eq!(error.kind(), ErrorKind::InvalidData);
    }

    #[test]
    fn bounded_reader_rejects_oversized_headers() {
        let header = format!(
            "X-Header: {}\r\nContent-Length: 0\r\n\r\n",
            "x".repeat(9 * 1024)
        );
        let mut reader = BoundedReader::new(Cursor::new(header.into_bytes()));
        let error = Message::read(&mut reader).expect_err("oversized header must be rejected");
        assert_eq!(error.kind(), ErrorKind::InvalidData);
    }

    #[test]
    fn watcher_paths_remain_pending_until_acknowledged() {
        let path = PathBuf::from("/workspace/.lint4d.toml");
        let mut registration = FileWatcherRegistration::with_relative_pattern_support(true);
        let paths = registration.new_paths(vec![path.clone()]);
        let request_id = RequestId::from("watcher-ack".to_string());
        registration.mark_pending(request_id.clone(), &paths);

        assert!(registration.new_paths(vec![path.clone()]).is_empty());
        assert!(registration.acknowledged_paths.is_empty());
        assert_eq!(
            registration.handle_response(&Response::new_ok(request_id, serde_json::Value::Null)),
            Some(true)
        );
        assert!(
            registration
                .acknowledged_paths
                .contains(&super::watcher_path_key(&path))
        );
        assert!(registration.new_paths(vec![path]).is_empty());
    }

    #[test]
    fn rejected_watcher_paths_are_retried_then_explicitly_degraded() {
        let path = PathBuf::from("/workspace/.lint4d.toml");
        let key = super::watcher_path_key(&path);
        let mut registration = FileWatcherRegistration::with_relative_pattern_support(true);

        for attempt in 0..MAX_WATCHER_REGISTRATION_RETRIES {
            let paths = registration.new_paths(vec![path.clone()]);
            assert_eq!(paths, vec![path.clone()]);
            let request_id = RequestId::from(format!("watcher-reject-{attempt}"));
            registration.mark_pending(request_id.clone(), &paths);
            assert_eq!(
                registration.handle_response(&Response::new_err(
                    request_id,
                    -32603,
                    "rejected".to_string(),
                )),
                Some(false)
            );
            assert!(registration.acknowledged_paths.is_empty());
            if attempt + 1 < MAX_WATCHER_REGISTRATION_RETRIES {
                assert!(registration.new_paths(vec![path.clone()]).len() == 1);
            }
        }

        assert!(registration.new_paths(vec![path]).is_empty());
        assert!(registration.degraded_paths.contains(&key));
        assert!(registration.acknowledged_paths.is_empty());
    }

    #[test]
    fn watcher_allowance_reaches_eligible_paths_after_degraded_candidates() {
        let temp = tempfile::tempdir().expect("temporary workspace");
        let root = temp.path().join("workspace");
        let mut workspace = test_workspace(vec![root.clone()], Default::default());
        let source = "unit Main; interface implementation end.\n";

        for index in 0..=MAX_CONFIGURATION_WATCH_PATHS {
            let directory = root.join(format!("directory-{index:03}"));
            fs::create_dir_all(&directory).expect("candidate directory");
            let source_path = directory.join("Main.pas");
            fs::write(&source_path, source).expect("source file");
            fs::write(
                directory.join(format!("Project-{index:03}.dproj")),
                "<Project><PropertyGroup><MainSource>Main.pas</MainSource></PropertyGroup></Project>",
            )
            .expect("project file");
            let uri = Url::from_file_path(source_path).expect("source URI");
            workspace
                .project_context(&uri)
                .expect("project context should be discoverable");
        }

        let all_paths = workspace.configuration_watch_paths();
        assert!(
            all_paths.len() > MAX_CONFIGURATION_WATCH_PATHS,
            "workspace must expose more candidates than the explicit allowance"
        );

        let mut registration = FileWatcherRegistration::with_relative_pattern_support(true);
        for attempt in 0..MAX_WATCHER_REGISTRATION_RETRIES {
            let paths = registration.new_paths(all_paths.clone());
            assert_eq!(paths.len(), MAX_CONFIGURATION_WATCH_PATHS);
            let request_id = RequestId::from(format!("watcher-page-reject-{attempt}"));
            registration.mark_pending(request_id.clone(), &paths);
            assert_eq!(
                registration.handle_response(&Response::new_err(
                    request_id,
                    -32603,
                    "rejected".to_string(),
                )),
                Some(false)
            );
        }

        let next_page = registration.new_paths(all_paths);
        assert!(
            !next_page.is_empty(),
            "degraded candidates must not strand later eligible paths"
        );
    }

    #[test]
    fn delivery_rejects_a_generation_change_after_compute() {
        let temp = tempfile::tempdir().expect("temporary workspace");
        let root = temp.path().join("fixture");
        let main = root.join("Main.pas");
        let project_a = root.join("A.dproj");
        let project_b = root.join("B.dproj");
        fs::create_dir_all(&root).expect("fixture directory");
        fs::write(&main, "unit Main; interface implementation end.\n").expect("source");
        for project in [&project_a, &project_b] {
            fs::write(
                project,
                "<Project><PropertyGroup><MainSource>Main.pas</MainSource></PropertyGroup></Project>",
            )
            .expect("project");
        }

        let main_uri = Url::from_file_path(&main).expect("source URI");
        let project_a_uri = Url::from_file_path(&project_a).expect("project A URI");
        let project_b_uri = Url::from_file_path(&project_b).expect("project B URI");
        let (server, client) = Connection::memory();
        let mut workspace = test_workspace(vec![root], Default::default());
        workspace
            .select_project(&main_uri, Some(&project_a_uri))
            .expect("select project A");
        let source_generation = workspace.source_generation();
        let configuration_generation = workspace.configuration_generation();
        let control_id = RequestId::from("generation-boundary-control".to_string());
        deliver_analysis_result(
            &server,
            &mut workspace,
            AnalysisResult {
                id: AnalysisJobId::Client(AnalysisComputationId(0)),
                source_generation,
                configuration_generation,
                records: Vec::new(),
                value: AnalysisResultValue::Prepare(Ok(PrepareRenameResponse::Range(Range::new(
                    Position::new(0, 0),
                    Position::new(0, 1),
                )))),
            },
            Some(control_id.clone()),
        )
        .expect("unchanged result response");

        let Message::Response(control) = client.receiver.recv().expect("control response") else {
            panic!("expected a control response");
        };
        assert_eq!(control.id, control_id);
        assert!(
            control.error.is_none(),
            "unchanged generation must deliver successfully: {control:?}"
        );
        assert!(
            control.result.is_some(),
            "successful result must be delivered"
        );

        let stale_source_generation = workspace.source_generation();
        let stale_configuration_generation = workspace.configuration_generation();
        workspace
            .select_project(&main_uri, Some(&project_b_uri))
            .expect("select project B");
        assert_ne!(
            workspace.configuration_generation(),
            stale_configuration_generation,
            "project selection must change the configuration generation"
        );
        assert_ne!(
            workspace.source_generation(),
            stale_source_generation,
            "project selection must change the source generation"
        );

        let id = RequestId::from("generation-boundary".to_string());
        deliver_analysis_result(
            &server,
            &mut workspace,
            AnalysisResult {
                id: AnalysisJobId::Client(AnalysisComputationId(0)),
                source_generation: stale_source_generation,
                configuration_generation: stale_configuration_generation,
                records: Vec::new(),
                value: AnalysisResultValue::Prepare(Ok(PrepareRenameResponse::Range(Range::new(
                    Position::new(0, 0),
                    Position::new(0, 1),
                )))),
            },
            Some(id.clone()),
        )
        .expect("stale result response");

        let Message::Response(response) = client.receiver.recv().expect("delivery response") else {
            panic!("expected a response");
        };
        assert_eq!(response.id, id);
        assert_eq!(response.error.expect("stale result error").code, -32803);
    }

    #[test]
    fn analysis_workers_queue_requests_beyond_the_worker_bound() {
        let temp = tempfile::tempdir().expect("temporary workspace");
        let root = temp.path().join("fixture");
        let first = root.join("First.pas");
        let source = "unit First;\ninterface\nprocedure VisibleThing;\nimplementation\nend.\n";
        fs::create_dir_all(&root).expect("fixture directory");
        fs::write(&first, source).expect("first source");

        let first_uri = Url::from_file_path(&first).expect("first URI");
        let mut workspace = test_workspace(vec![root], Default::default());
        let mut jobs = AnalysisJobs::new();

        let (first_ready_sender, first_ready_receiver) = mpsc::channel();
        let (first_release_sender, first_release_receiver) = mpsc::channel();
        install_snapshot_priority_barrier(
            first_uri.clone(),
            first_ready_sender,
            first_release_receiver,
        );
        jobs.start(
            RequestId::from("busy-first".to_string()),
            AnalysisRequest::SemanticTokens {
                uri: first_uri.clone(),
                range: None,
            },
            &workspace,
            symbol_client_features(),
        )
        .expect("first worker must start");
        first_ready_receiver
            .recv_timeout(Duration::from_secs(5))
            .expect("first worker must reach the snapshot barrier");

        jobs.start(
            RequestId::from("busy-second".to_string()),
            AnalysisRequest::SemanticTokens {
                uri: first_uri.clone(),
                range: Some(Range::new(Position::new(0, 0), Position::new(0, 1))),
            },
            &workspace,
            symbol_client_features(),
        )
        .expect("second worker must start");
        assert_eq!(jobs.pending.len(), 2, "both workers must remain active");

        jobs.start(
            RequestId::from("busy-queued".to_string()),
            AnalysisRequest::SemanticTokens {
                uri: first_uri,
                range: Some(Range::new(Position::new(1, 0), Position::new(1, 1))),
            },
            &workspace,
            symbol_client_features(),
        )
        .expect("requests beyond the active worker bound must be queued");
        assert_eq!(
            jobs.queue.len(),
            1,
            "the third request must wait in the queue"
        );

        let (server, client) = Connection::memory();
        first_release_sender.send(()).expect("release first worker");
        let deadline = Instant::now() + Duration::from_secs(5);
        while !jobs.is_empty() && Instant::now() < deadline {
            jobs.poll(&server, &mut workspace)
                .expect("poll analysis requests");
            while let Ok(message) = client.receiver.try_recv() {
                assert!(matches!(message, Message::Response(_)));
            }
            if !jobs.is_empty() {
                thread::sleep(Duration::from_millis(1));
            }
        }

        assert!(jobs.is_empty(), "all admitted requests must be drained");
    }

    #[test]
    fn identical_semantic_token_requests_share_one_scheduled_computation() {
        let temp = tempfile::tempdir().expect("temporary workspace");
        let root = temp.path().join("fixture");
        let source_path = root.join("Tokens.pas");
        let source = "unit Tokens;\ninterface\nconst Value = 1;\nimplementation\nend.\n";
        fs::create_dir_all(&root).expect("fixture directory");
        fs::write(&source_path, source).expect("source");

        let uri = Url::from_file_path(&source_path).expect("source URI");
        let workspace = test_workspace(vec![root], Default::default());
        let mut jobs = AnalysisJobs::new();
        let first_id = RequestId::from("semantic-token-cache-first".to_string());
        jobs.start(
            first_id.clone(),
            AnalysisRequest::SemanticTokens {
                uri: uri.clone(),
                range: None,
            },
            &workspace,
            symbol_client_features(),
        )
        .expect("first semantic-token request must start");

        let second_id = RequestId::from("semantic-token-cache-second".to_string());
        jobs.start(
            second_id.clone(),
            AnalysisRequest::SemanticTokens { uri, range: None },
            &workspace,
            symbol_client_features(),
        )
        .expect("identical semantic-token request must attach");

        assert_eq!(
            jobs.pending.len(),
            1,
            "identical requests must share a worker"
        );
        let computation_id = jobs
            .request_to_job
            .get(&first_id)
            .copied()
            .expect("first request mapping");
        assert_eq!(jobs.request_to_job.get(&second_id), Some(&computation_id));
        assert_eq!(
            jobs.pending
                .get(&computation_id)
                .expect("shared pending computation")
                .client_ids,
            vec![first_id.clone(), second_id]
        );

        let result = receive_analysis_result(&mut jobs, &first_id);
        assert_semantic_tokens_were_computed(&result);
    }

    #[test]
    fn semantic_token_worker_honors_cancellation_during_snapshot_construction() {
        let temp = tempfile::tempdir().expect("temporary workspace");
        let root = temp.path().join("fixture");
        let source_path = root.join("Tokens.pas");
        let source = "unit Tokens;\ninterface\nconst Value = 1;\nimplementation\nend.\n";
        fs::create_dir_all(&root).expect("fixture directory");
        fs::write(&source_path, source).expect("source");

        let uri = Url::from_file_path(&source_path).expect("source URI");
        let workspace = test_workspace(vec![root], Default::default());
        let mut jobs = AnalysisJobs::new();
        let (ready_sender, ready_receiver) = mpsc::channel();
        let (release_sender, release_receiver) = mpsc::channel();
        install_snapshot_priority_barrier(uri.clone(), ready_sender, release_receiver);
        let id = RequestId::from("cancelled-semantic-tokens".to_string());
        jobs.start(
            id.clone(),
            AnalysisRequest::SemanticTokens { uri, range: None },
            &workspace,
            symbol_client_features(),
        )
        .expect("semantic token worker must start");
        ready_receiver
            .recv_timeout(Duration::from_secs(5))
            .expect("semantic token worker must reach the snapshot barrier");
        let computation_id = jobs
            .request_to_job
            .get(&id)
            .copied()
            .expect("semantic token request mapping");
        let (cancel_server, cancel_client) = Connection::memory();
        jobs.cancel(&cancel_server, &id)
            .expect("cancel semantic token request");
        let Message::Response(cancel_response) = cancel_client
            .receiver
            .recv()
            .expect("cancellation response")
        else {
            panic!("expected a cancellation response");
        };
        assert_eq!(cancel_response.id, id);
        assert_eq!(
            cancel_response.error.expect("cancellation error").code,
            -32800
        );
        release_sender.send(()).expect("release semantic worker");

        let result = jobs.receiver.recv().expect("analysis worker result");
        let pending = jobs
            .pending
            .remove(&computation_id)
            .expect("pending semantic token job");
        assert!(
            pending.handle.join().is_ok(),
            "analysis worker must exit cleanly"
        );
        assert_eq!(result.id, AnalysisJobId::Client(computation_id));
        match result.value {
            AnalysisResultValue::SemanticTokens(Err(error)) => {
                assert_eq!(error, "request cancelled")
            }
            _ => panic!("expected cancellation from in-flight semantic token request"),
        }
    }

    #[test]
    fn delivery_rejects_a_computed_symbol_result_after_an_overlay_change() {
        let temp = tempfile::tempdir().expect("temporary workspace");
        let root = temp.path().join("fixture");
        let main = root.join("Main.pas");
        let original = "unit Main;\ninterface\nprocedure VisibleThing;\nimplementation\nend.\n";
        let changed = "unit Main;\ninterface\nprocedure ChangedThing;\nimplementation\nend.\n";
        fs::create_dir_all(&root).expect("fixture directory");
        fs::write(&main, original).expect("source");

        let main_uri = Url::from_file_path(&main).expect("source URI");
        let mut workspace = test_workspace(vec![root], Default::default());
        workspace
            .open_document(main_uri.clone(), original.to_string(), 1)
            .expect("open overlay");
        let mut jobs = AnalysisJobs::new();

        let control_id = RequestId::from("symbol-overlay-control".to_string());
        jobs.start(
            control_id.clone(),
            AnalysisRequest::WorkspaceSymbols {
                query: "VisibleThing".to_string(),
            },
            &workspace,
            symbol_client_features(),
        )
        .expect("start control symbol request");
        let control = receive_analysis_result(&mut jobs, &control_id);
        assert_workspace_symbols_were_computed(&control);
        let (server, client) = Connection::memory();
        deliver_analysis_result(&server, &mut workspace, control, Some(control_id.clone()))
            .expect("deliver control result");
        let Message::Response(control_response) = client.receiver.recv().expect("control response")
        else {
            panic!("expected a control response");
        };
        assert!(
            control_response.error.is_none(),
            "unchanged computed symbol input must deliver: {control_response:?}"
        );

        let stale_id = RequestId::from("symbol-overlay-stale".to_string());
        jobs.start(
            stale_id.clone(),
            AnalysisRequest::WorkspaceSymbols {
                query: "VisibleThing".to_string(),
            },
            &workspace,
            symbol_client_features(),
        )
        .expect("start stale symbol request");
        let stale = receive_analysis_result(&mut jobs, &stale_id);
        assert_workspace_symbols_were_computed(&stale);

        // The worker has completed and its result is now queued. Change the
        // authoritative overlay before exercising the delivery boundary.
        workspace
            .change_document(main_uri, changed.to_string(), 2)
            .expect("change overlay");
        let (server, client) = Connection::memory();
        deliver_analysis_result(&server, &mut workspace, stale, Some(stale_id.clone()))
            .expect("deliver stale result");
        let Message::Response(response) = client.receiver.recv().expect("stale response") else {
            panic!("expected a stale response");
        };
        assert_eq!(
            response
                .error
                .expect("changed overlay must reject result")
                .code,
            -32803
        );
    }

    #[test]
    fn delivery_rejects_a_computed_folding_result_after_an_overlay_change() {
        let temp = tempfile::tempdir().expect("temporary workspace");
        let root = temp.path().join("fixture");
        let main = root.join("Main.pas");
        let original = "unit Main;\ninterface\nprocedure VisibleThing;\nimplementation\nprocedure VisibleThing;\nbegin\nend;\nend.\n";
        let changed = "unit Main;\ninterface\nprocedure ChangedThing;\nimplementation\nprocedure ChangedThing;\nbegin\nend;\nend.\n";
        fs::create_dir_all(&root).expect("fixture directory");
        fs::write(&main, original).expect("source");

        let main_uri = Url::from_file_path(&main).expect("source URI");
        let mut workspace = test_workspace(vec![root], Default::default());
        workspace
            .open_document(main_uri.clone(), original.to_string(), 1)
            .expect("open overlay");
        let mut jobs = AnalysisJobs::new();

        let control_id = RequestId::from("folding-overlay-control".to_string());
        jobs.start(
            control_id.clone(),
            AnalysisRequest::FoldingRanges {
                uri: main_uri.clone(),
            },
            &workspace,
            symbol_client_features(),
        )
        .expect("start folding control request");
        let control = receive_analysis_result(&mut jobs, &control_id);
        assert_folding_ranges_were_computed(&control);
        deliver_successfully(&mut workspace, control_id, control);

        let stale_id = RequestId::from("folding-overlay-stale".to_string());
        jobs.start(
            stale_id.clone(),
            AnalysisRequest::FoldingRanges {
                uri: main_uri.clone(),
            },
            &workspace,
            symbol_client_features(),
        )
        .expect("start stale folding request");
        let stale = receive_analysis_result(&mut jobs, &stale_id);
        assert_folding_ranges_were_computed(&stale);

        workspace
            .change_document(main_uri, changed.to_string(), 2)
            .expect("change overlay");
        let (server, client) = Connection::memory();
        deliver_analysis_result(&server, &mut workspace, stale, Some(stale_id.clone()))
            .expect("deliver stale folding result");
        let Message::Response(response) = client.receiver.recv().expect("stale response") else {
            panic!("expected a stale response");
        };
        assert_eq!(
            response
                .error
                .expect("changed overlay must reject folding result")
                .code,
            -32803
        );
    }

    #[test]
    fn delivery_rejects_a_computed_hover_result_after_an_overlay_change() {
        let temp = tempfile::tempdir().expect("temporary workspace");
        let root = temp.path().join("fixture");
        let main = root.join("Main.pas");
        let original = "unit Main;\ninterface\nprocedure VisibleThing;\nimplementation\nend.\n";
        let changed = "unit Main;\ninterface\nprocedure ChangedThing;\nimplementation\nend.\n";
        fs::create_dir_all(&root).expect("fixture directory");
        fs::write(&main, original).expect("source");

        let main_uri = Url::from_file_path(&main).expect("source URI");
        let mut workspace = Workspace::new(vec![root], Default::default());
        workspace
            .open_document(main_uri.clone(), original.to_string(), 1)
            .expect("open overlay");
        let mut jobs = AnalysisJobs::new();

        let control_id = RequestId::from("hover-overlay-control".to_string());
        jobs.start(
            control_id.clone(),
            AnalysisRequest::Hover {
                uri: main_uri.clone(),
                position: Position::new(2, 10),
                format: MarkupKind::PlainText,
            },
            &workspace,
            symbol_client_features(),
        )
        .expect("start hover control request");
        let control = receive_analysis_result(&mut jobs, &control_id);
        assert_hover_was_computed(&control);
        deliver_successfully(&mut workspace, control_id, control);

        let id = RequestId::from("hover-overlay-stale".to_string());
        jobs.start(
            id.clone(),
            AnalysisRequest::Hover {
                uri: main_uri.clone(),
                position: Position::new(2, 10),
                format: MarkupKind::PlainText,
            },
            &workspace,
            symbol_client_features(),
        )
        .expect("start hover request");
        let result = receive_analysis_result(&mut jobs, &id);
        assert_hover_was_computed(&result);

        workspace
            .change_document(main_uri, changed.to_string(), 2)
            .expect("change overlay");
        let (server, client) = Connection::memory();
        deliver_analysis_result(&server, &mut workspace, result, Some(id.clone()))
            .expect("deliver stale hover result");
        let Message::Response(response) = client.receiver.recv().expect("stale response") else {
            panic!("expected a stale response");
        };
        assert_eq!(
            response
                .error
                .expect("changed hover overlay must reject result")
                .code,
            -32803
        );
    }

    #[test]
    fn delivery_rejects_a_computed_type_definition_result_after_an_overlay_change() {
        let temp = tempfile::tempdir().expect("temporary workspace");
        let root = temp.path().join("fixture");
        let provider = root.join("Provider.pas");
        let consumer = root.join("Consumer.pas");
        let provider_source =
            "unit Provider;\ninterface\ntype\n  TOverlay = class end;\nimplementation\nend.\n";
        let changed_provider =
            "unit Provider;\ninterface\ntype\n  TChanged = class end;\nimplementation\nend.\n";
        let consumer_source = "unit Consumer;\ninterface\nuses Provider;\nimplementation\nprocedure Run;\nvar Item: TOverlay;\nbegin\n  Item := nil;\nend;\nend.\n";
        fs::create_dir_all(&root).expect("fixture directory");
        fs::write(&provider, provider_source).expect("provider source");
        fs::write(&consumer, consumer_source).expect("consumer source");

        let provider_uri = Url::from_file_path(&provider).expect("provider URI");
        let consumer_uri = Url::from_file_path(&consumer).expect("consumer URI");
        let mut workspace = Workspace::new(vec![root], Default::default());
        workspace
            .open_document(provider_uri.clone(), provider_source.to_string(), 1)
            .expect("open provider overlay");
        let mut jobs = AnalysisJobs::new();

        let control_id = RequestId::from("type-definition-overlay-control".to_string());
        jobs.start(
            control_id.clone(),
            AnalysisRequest::TypeDefinitions {
                uri: consumer_uri.clone(),
                position: Position::new(5, 4),
            },
            &workspace,
            symbol_client_features(),
        )
        .expect("start type-definition control request");
        let control = receive_analysis_result(&mut jobs, &control_id);
        assert_type_definitions_were_computed(&control);
        deliver_successfully(&mut workspace, control_id, control);

        let stale_id = RequestId::from("type-definition-overlay-stale".to_string());
        jobs.start(
            stale_id.clone(),
            AnalysisRequest::TypeDefinitions {
                uri: consumer_uri,
                position: Position::new(5, 4),
            },
            &workspace,
            symbol_client_features(),
        )
        .expect("start stale type-definition request");
        let stale = receive_analysis_result(&mut jobs, &stale_id);
        assert_type_definitions_were_computed(&stale);

        workspace
            .change_document(provider_uri, changed_provider.to_string(), 2)
            .expect("change provider overlay");
        let (server, client) = Connection::memory();
        deliver_analysis_result(&server, &mut workspace, stale, Some(stale_id.clone()))
            .expect("deliver stale type-definition result");
        let Message::Response(response) = client.receiver.recv().expect("stale response") else {
            panic!("expected a stale response");
        };
        assert_eq!(
            response
                .error
                .expect("changed type-definition overlay must reject result")
                .code,
            -32803
        );
    }

    #[test]
    fn delivery_rejects_a_computed_completion_result_after_an_overlay_change() {
        let temp = tempfile::tempdir().expect("temporary workspace");
        let root = temp.path().join("fixture");
        let main = root.join("Main.pas");
        let original = "unit Main;\ninterface\nimplementation\nprocedure Run;\nvar\n  LocalName: Integer;\nbegin\n  Loc;\nend;\nend.\n";
        let changed = original.replace("LocalName", "ChangedName");
        fs::create_dir_all(&root).expect("fixture directory");
        fs::write(&main, original).expect("source");

        let main_uri = Url::from_file_path(&main).expect("source URI");
        let mut workspace = Workspace::new(vec![root], Default::default());
        workspace
            .open_document(main_uri.clone(), original.to_string(), 1)
            .expect("open overlay");
        let mut jobs = AnalysisJobs::new();

        let control_id = RequestId::from("completion-overlay-control".to_string());
        jobs.start(
            control_id.clone(),
            AnalysisRequest::Completion {
                uri: main_uri.clone(),
                position: Position::new(7, 5),
                format: MarkupKind::Markdown,
                resolve_documentation: false,
                resolve_detail: false,
            },
            &workspace,
            symbol_client_features(),
        )
        .expect("start completion control request");
        let control = receive_analysis_result(&mut jobs, &control_id);
        assert_completion_was_computed(&control);
        deliver_successfully(&mut workspace, control_id, control);

        let stale_id = RequestId::from("completion-overlay-stale".to_string());
        jobs.start(
            stale_id.clone(),
            AnalysisRequest::Completion {
                uri: main_uri.clone(),
                position: Position::new(7, 5),
                format: MarkupKind::Markdown,
                resolve_documentation: false,
                resolve_detail: false,
            },
            &workspace,
            symbol_client_features(),
        )
        .expect("start stale completion request");
        let stale = receive_analysis_result(&mut jobs, &stale_id);
        assert_completion_was_computed(&stale);

        workspace
            .change_document(main_uri, changed, 2)
            .expect("change overlay");
        let (server, client) = Connection::memory();
        deliver_analysis_result(&server, &mut workspace, stale, Some(stale_id.clone()))
            .expect("deliver stale completion result");
        let Message::Response(response) = client.receiver.recv().expect("stale response") else {
            panic!("expected a stale response");
        };
        assert_eq!(
            response
                .error
                .expect("changed completion overlay must reject result")
                .code,
            -32803
        );
    }

    #[test]
    fn delivery_rejects_a_computed_signature_help_result_after_an_overlay_change() {
        let temp = tempfile::tempdir().expect("temporary workspace");
        let root = temp.path().join("fixture");
        let main = root.join("Main.pas");
        let original = "unit Main;\ninterface\nprocedure Run(Value: Integer);\nimplementation\nprocedure Run(Value: Integer);\nbegin\nend;\nprocedure Caller;\nbegin\n  Run(1);\nend;\nend.\n";
        let changed = original.replace("Run", "Changed");
        fs::create_dir_all(&root).expect("fixture directory");
        fs::write(&main, original).expect("source");

        let main_uri = Url::from_file_path(&main).expect("source URI");
        let mut workspace = Workspace::new(vec![root], Default::default());
        workspace
            .open_document(main_uri.clone(), original.to_string(), 1)
            .expect("open overlay");
        let mut jobs = AnalysisJobs::new();

        let control_id = RequestId::from("signature-help-overlay-control".to_string());
        jobs.start(
            control_id.clone(),
            AnalysisRequest::SignatureHelp {
                uri: main_uri.clone(),
                position: Position::new(9, 6),
                format: MarkupKind::Markdown,
            },
            &workspace,
            symbol_client_features(),
        )
        .expect("start signature-help control request");
        let control = receive_analysis_result(&mut jobs, &control_id);
        assert_signature_help_was_computed(&control);
        deliver_successfully(&mut workspace, control_id, control);

        let stale_id = RequestId::from("signature-help-overlay-stale".to_string());
        jobs.start(
            stale_id.clone(),
            AnalysisRequest::SignatureHelp {
                uri: main_uri.clone(),
                position: Position::new(9, 6),
                format: MarkupKind::Markdown,
            },
            &workspace,
            symbol_client_features(),
        )
        .expect("start stale signature-help request");
        let stale = receive_analysis_result(&mut jobs, &stale_id);
        assert_signature_help_was_computed(&stale);

        workspace
            .change_document(main_uri, changed, 2)
            .expect("change overlay");
        let (server, client) = Connection::memory();
        deliver_analysis_result(&server, &mut workspace, stale, Some(stale_id.clone()))
            .expect("deliver stale signature-help result");
        let Message::Response(response) = client.receiver.recv().expect("stale response") else {
            panic!("expected a stale response");
        };
        assert_eq!(
            response
                .error
                .expect("changed signature-help overlay must reject result")
                .code,
            -32803
        );
    }

    #[test]
    fn delivery_rejects_a_computed_symbol_result_after_a_project_switch() {
        let temp = tempfile::tempdir().expect("temporary workspace");
        let root = temp.path().join("fixture");
        let main = root.join("Main.pas");
        let project_a = root.join("A.dproj");
        let project_b = root.join("B.dproj");
        let source = "unit Main;\ninterface\nprocedure VisibleThing;\nimplementation\nend.\n";
        let project =
            "<Project><PropertyGroup><MainSource>Main.pas</MainSource></PropertyGroup></Project>";
        fs::create_dir_all(&root).expect("fixture directory");
        fs::write(&main, source).expect("source");
        fs::write(&project_a, project).expect("project A");
        fs::write(&project_b, project).expect("project B");

        let main_uri = Url::from_file_path(&main).expect("source URI");
        let project_a_uri = Url::from_file_path(&project_a).expect("project A URI");
        let project_b_uri = Url::from_file_path(&project_b).expect("project B URI");
        let mut workspace = test_workspace(vec![root], Default::default());
        workspace
            .select_project(&main_uri, Some(&project_a_uri))
            .expect("select project A");
        let mut jobs = AnalysisJobs::new();

        let control_id = RequestId::from("symbol-project-control".to_string());
        jobs.start(
            control_id.clone(),
            AnalysisRequest::WorkspaceSymbols {
                query: "VisibleThing".to_string(),
            },
            &workspace,
            symbol_client_features(),
        )
        .expect("start control symbol request");
        let control = receive_analysis_result(&mut jobs, &control_id);
        assert_workspace_symbols_were_computed(&control);
        let (server, client) = Connection::memory();
        deliver_analysis_result(&server, &mut workspace, control, Some(control_id.clone()))
            .expect("deliver control result");
        let Message::Response(control_response) = client.receiver.recv().expect("control response")
        else {
            panic!("expected a control response");
        };
        assert!(
            control_response.error.is_none(),
            "unchanged computed symbol input must deliver: {control_response:?}"
        );

        let stale_id = RequestId::from("symbol-project-stale".to_string());
        jobs.start(
            stale_id.clone(),
            AnalysisRequest::WorkspaceSymbols {
                query: "VisibleThing".to_string(),
            },
            &workspace,
            symbol_client_features(),
        )
        .expect("start stale symbol request");
        let stale = receive_analysis_result(&mut jobs, &stale_id);
        assert_workspace_symbols_were_computed(&stale);
        let stale_source_generation = stale.source_generation;
        let stale_configuration_generation = stale.configuration_generation;

        // The worker has completed and its result is now queued. Switch the
        // selected project before exercising the delivery boundary.
        workspace
            .select_project(&main_uri, Some(&project_b_uri))
            .expect("switch to project B");
        assert_ne!(
            workspace.source_generation(),
            stale_source_generation,
            "project switch must invalidate source generation"
        );
        assert_ne!(
            workspace.configuration_generation(),
            stale_configuration_generation,
            "project switch must invalidate configuration generation"
        );
        let (server, client) = Connection::memory();
        deliver_analysis_result(&server, &mut workspace, stale, Some(stale_id.clone()))
            .expect("deliver stale result");
        let Message::Response(response) = client.receiver.recv().expect("stale response") else {
            panic!("expected a stale response");
        };
        assert_eq!(
            response
                .error
                .expect("project switch must reject result")
                .code,
            -32803
        );
    }

    #[test]
    fn delivery_rejects_a_computed_reference_result_after_an_overlay_change() {
        let temp = tempfile::tempdir().expect("temporary workspace");
        let root = temp.path().join("fixture");
        let provider = root.join("Provider.pas");
        let consumer = root.join("Consumer.pas");
        let provider_source =
            "unit Provider;\ninterface\nconst SharedValue = 1;\nimplementation\nend.\n";
        let consumer_source = "unit Consumer;\ninterface\nuses Provider;\nimplementation\nprocedure Run;\nbegin\n  Log(SharedValue);\nend;\nend.\n";
        let changed_consumer = consumer_source.replace("SharedValue", "ChangedValue");
        fs::create_dir_all(&root).expect("fixture directory");
        fs::write(&provider, provider_source).expect("provider source");
        fs::write(&consumer, consumer_source).expect("consumer source");

        let provider_uri = Url::from_file_path(&provider).expect("provider URI");
        let consumer_uri = Url::from_file_path(&consumer).expect("consumer URI");
        let mut workspace = test_workspace(vec![root], Default::default());
        workspace
            .open_document(consumer_uri.clone(), consumer_source.to_string(), 1)
            .expect("open consumer overlay");
        let mut jobs = AnalysisJobs::new();

        let control_id = RequestId::from("reference-overlay-control".to_string());
        jobs.start(
            control_id.clone(),
            AnalysisRequest::References {
                uri: provider_uri.clone(),
                position: Position::new(2, 6),
                include_declaration: false,
            },
            &workspace,
            symbol_client_features(),
        )
        .expect("start reference control request");
        let control = receive_analysis_result(&mut jobs, &control_id);
        assert!(matches!(
            &control.value,
            AnalysisResultValue::References(Ok(locations)) if locations.len() == 1
        ));
        deliver_successfully(&mut workspace, control_id, control);

        let id = RequestId::from("reference-overlay-stale".to_string());
        jobs.start(
            id.clone(),
            AnalysisRequest::References {
                uri: provider_uri,
                position: Position::new(2, 6),
                include_declaration: false,
            },
            &workspace,
            symbol_client_features(),
        )
        .expect("start reference request");
        let result = receive_analysis_result(&mut jobs, &id);
        match &result.value {
            AnalysisResultValue::References(Ok(locations)) => {
                assert_eq!(
                    locations.len(),
                    1,
                    "reference result must be completed before mutation"
                );
            }
            _ => panic!("expected completed reference result"),
        }
        assert!(
            !result.records.is_empty(),
            "reference result must carry its read set"
        );

        workspace
            .change_document(consumer_uri, changed_consumer, 2)
            .expect("change consumer overlay");
        let (server, client) = Connection::memory();
        deliver_analysis_result(&server, &mut workspace, result, Some(id.clone()))
            .expect("deliver stale result");
        let Message::Response(response) = client.receiver.recv().expect("stale response") else {
            panic!("expected a stale response");
        };
        assert_eq!(
            response
                .error
                .expect("changed reference overlay must reject result")
                .code,
            -32803
        );
    }

    #[test]
    fn delivery_rejects_a_computed_highlight_result_after_an_overlay_change() {
        let temp = tempfile::tempdir().expect("temporary workspace");
        let root = temp.path().join("fixture");
        let main = root.join("Main.pas");
        let original = "unit Main;\ninterface\nconst Value = 1;\nimplementation\nprocedure Run;\nbegin\n  Log(Value);\nend;\nend.\n";
        let changed = original.replace("Log(Value);", "Log(OtherValue);");
        fs::create_dir_all(&root).expect("fixture directory");
        fs::write(&main, original).expect("source");

        let main_uri = Url::from_file_path(&main).expect("source URI");
        let mut workspace = test_workspace(vec![root], Default::default());
        workspace
            .open_document(main_uri.clone(), original.to_string(), 1)
            .expect("open overlay");
        let mut jobs = AnalysisJobs::new();

        let control_id = RequestId::from("highlight-overlay-control".to_string());
        jobs.start(
            control_id.clone(),
            AnalysisRequest::DocumentHighlights {
                uri: main_uri.clone(),
                position: Position::new(2, 6),
            },
            &workspace,
            symbol_client_features(),
        )
        .expect("start highlight control request");
        let control = receive_analysis_result(&mut jobs, &control_id);
        assert!(matches!(
            &control.value,
            AnalysisResultValue::DocumentHighlights(Ok(highlights)) if highlights.len() == 2
        ));
        deliver_successfully(&mut workspace, control_id, control);

        let id = RequestId::from("highlight-overlay-stale".to_string());
        jobs.start(
            id.clone(),
            AnalysisRequest::DocumentHighlights {
                uri: main_uri.clone(),
                position: Position::new(2, 6),
            },
            &workspace,
            symbol_client_features(),
        )
        .expect("start highlight request");
        let result = receive_analysis_result(&mut jobs, &id);
        match &result.value {
            AnalysisResultValue::DocumentHighlights(Ok(highlights)) => {
                assert_eq!(
                    highlights.len(),
                    2,
                    "highlight result must be completed before mutation"
                );
            }
            _ => panic!("expected completed highlight result"),
        }
        assert!(
            !result.records.is_empty(),
            "highlight result must carry its read set"
        );

        workspace
            .change_document(main_uri, changed, 2)
            .expect("change overlay");
        let (server, client) = Connection::memory();
        deliver_analysis_result(&server, &mut workspace, result, Some(id.clone()))
            .expect("deliver stale result");
        let Message::Response(response) = client.receiver.recv().expect("stale response") else {
            panic!("expected a stale response");
        };
        assert_eq!(
            response
                .error
                .expect("changed highlight overlay must reject result")
                .code,
            -32803
        );
    }

    #[test]
    fn delivery_rejects_a_computed_semantic_token_result_after_an_overlay_change() {
        let temp = tempfile::tempdir().expect("temporary workspace");
        let root = temp.path().join("fixture");
        let main = root.join("Main.pas");
        let original = "unit Main;\ninterface\nconst Value = 1;\nimplementation\nprocedure Run;\nbegin\n  Log(Value);\nend;\nend.\n";
        let changed = original.replace("Value", "ChangedValue");
        fs::create_dir_all(&root).expect("fixture directory");
        fs::write(&main, original).expect("source");

        let main_uri = Url::from_file_path(&main).expect("source URI");
        let mut workspace = test_workspace(vec![root], Default::default());
        workspace
            .open_document(main_uri.clone(), original.to_string(), 1)
            .expect("open overlay");
        let mut jobs = AnalysisJobs::new();

        let control_id = RequestId::from("semantic-token-overlay-control".to_string());
        jobs.start(
            control_id.clone(),
            AnalysisRequest::SemanticTokens {
                uri: main_uri.clone(),
                range: None,
            },
            &workspace,
            symbol_client_features(),
        )
        .expect("start semantic-token control request");
        let control = receive_analysis_result(&mut jobs, &control_id);
        assert_semantic_tokens_were_computed(&control);
        deliver_successfully(&mut workspace, control_id, control);

        let stale_id = RequestId::from("semantic-token-overlay-stale".to_string());
        jobs.start(
            stale_id.clone(),
            AnalysisRequest::SemanticTokens {
                uri: main_uri.clone(),
                range: None,
            },
            &workspace,
            symbol_client_features(),
        )
        .expect("start stale semantic-token request");
        let stale = receive_analysis_result(&mut jobs, &stale_id);
        assert_semantic_tokens_were_computed(&stale);

        workspace
            .change_document(main_uri, changed, 2)
            .expect("change overlay");
        let (server, client) = Connection::memory();
        deliver_analysis_result(&server, &mut workspace, stale, Some(stale_id.clone()))
            .expect("deliver stale semantic-token result");
        let Message::Response(response) = client.receiver.recv().expect("stale response") else {
            panic!("expected a stale response");
        };
        assert_eq!(
            response
                .error
                .expect("changed semantic-token overlay must reject result")
                .code,
            -32803
        );
    }

    #[test]
    fn delivery_rejects_computed_reference_and_highlight_results_after_a_project_switch() {
        let temp = tempfile::tempdir().expect("temporary workspace");
        let root = temp.path().join("fixture");
        let main = root.join("Main.pas");
        let project_a = root.join("A.dproj");
        let project_b = root.join("B.dproj");
        let source = "unit Main;\ninterface\nconst Value = 1;\nimplementation\nprocedure Run;\nbegin\n  Log(Value);\nend;\nend.\n";
        let project =
            "<Project><PropertyGroup><MainSource>Main.pas</MainSource></PropertyGroup></Project>";
        fs::create_dir_all(&root).expect("fixture directory");
        fs::write(&main, source).expect("source");
        fs::write(&project_a, project).expect("project A");
        fs::write(&project_b, project).expect("project B");

        let main_uri = Url::from_file_path(&main).expect("source URI");
        let project_a_uri = Url::from_file_path(&project_a).expect("project A URI");
        let project_b_uri = Url::from_file_path(&project_b).expect("project B URI");
        let mut workspace = test_workspace(vec![root], Default::default());
        workspace
            .select_project(&main_uri, Some(&project_a_uri))
            .expect("select project A");
        let mut jobs = AnalysisJobs::new();

        let reference_control_id = RequestId::from("project-reference-control".to_string());
        jobs.start(
            reference_control_id.clone(),
            AnalysisRequest::References {
                uri: main_uri.clone(),
                position: Position::new(2, 6),
                include_declaration: true,
            },
            &workspace,
            symbol_client_features(),
        )
        .expect("start reference control request");
        let reference_control = receive_analysis_result(&mut jobs, &reference_control_id);
        assert!(matches!(
            &reference_control.value,
            AnalysisResultValue::References(Ok(locations)) if locations.len() == 2
        ));
        deliver_successfully(&mut workspace, reference_control_id, reference_control);

        let highlight_control_id = RequestId::from("project-highlight-control".to_string());
        jobs.start(
            highlight_control_id.clone(),
            AnalysisRequest::DocumentHighlights {
                uri: main_uri.clone(),
                position: Position::new(2, 6),
            },
            &workspace,
            symbol_client_features(),
        )
        .expect("start highlight control request");
        let highlight_control = receive_analysis_result(&mut jobs, &highlight_control_id);
        assert!(matches!(
            &highlight_control.value,
            AnalysisResultValue::DocumentHighlights(Ok(highlights)) if highlights.len() == 2
        ));
        deliver_successfully(&mut workspace, highlight_control_id, highlight_control);

        let semantic_control_id = RequestId::from("project-semantic-token-control".to_string());
        jobs.start(
            semantic_control_id.clone(),
            AnalysisRequest::SemanticTokens {
                uri: main_uri.clone(),
                range: None,
            },
            &workspace,
            symbol_client_features(),
        )
        .expect("start semantic-token control request");
        let semantic_control = receive_analysis_result(&mut jobs, &semantic_control_id);
        assert_semantic_tokens_were_computed(&semantic_control);
        deliver_successfully(&mut workspace, semantic_control_id, semantic_control);

        let reference_id = RequestId::from("project-reference-stale".to_string());
        jobs.start(
            reference_id.clone(),
            AnalysisRequest::References {
                uri: main_uri.clone(),
                position: Position::new(2, 6),
                include_declaration: true,
            },
            &workspace,
            symbol_client_features(),
        )
        .expect("start reference request");
        let reference = receive_analysis_result(&mut jobs, &reference_id);
        assert!(matches!(
            &reference.value,
            AnalysisResultValue::References(Ok(locations)) if locations.len() == 2
        ));

        let highlight_id = RequestId::from("project-highlight-stale".to_string());
        jobs.start(
            highlight_id.clone(),
            AnalysisRequest::DocumentHighlights {
                uri: main_uri.clone(),
                position: Position::new(2, 6),
            },
            &workspace,
            symbol_client_features(),
        )
        .expect("start highlight request");
        let highlight = receive_analysis_result(&mut jobs, &highlight_id);
        assert!(matches!(
            &highlight.value,
            AnalysisResultValue::DocumentHighlights(Ok(highlights)) if highlights.len() == 2
        ));

        let semantic_id = RequestId::from("project-semantic-token-stale".to_string());
        jobs.start(
            semantic_id.clone(),
            AnalysisRequest::SemanticTokens {
                uri: main_uri.clone(),
                range: None,
            },
            &workspace,
            symbol_client_features(),
        )
        .expect("start semantic-token request");
        let semantic = receive_analysis_result(&mut jobs, &semantic_id);
        assert_semantic_tokens_were_computed(&semantic);

        workspace
            .select_project(
                &Url::from_file_path(&main).expect("source URI"),
                Some(&project_b_uri),
            )
            .expect("switch to project B");

        for (id, result) in [
            (reference_id, reference),
            (highlight_id, highlight),
            (semantic_id, semantic),
        ] {
            let (server, client) = Connection::memory();
            deliver_analysis_result(&server, &mut workspace, result, Some(id))
                .expect("deliver stale result");
            let Message::Response(response) = client.receiver.recv().expect("stale response")
            else {
                panic!("expected a stale response");
            };
            assert_eq!(
                response
                    .error
                    .expect("project switch must reject query result")
                    .code,
                -32803
            );
        }
    }

    #[test]
    fn delivery_preserves_cancellation_from_candidate_membership_revalidation() {
        let temp = tempfile::tempdir().expect("temporary workspace");
        let root = temp.path().join("fixture");
        let provider = root.join("Provider.pas");
        let source = "unit Provider;\ninterface\nconst SharedValue = 1;\nimplementation\nend.\n";
        fs::create_dir_all(&root).expect("fixture directory");
        fs::write(&provider, source).expect("provider source");

        let provider_uri = Url::from_file_path(&provider).expect("provider URI");
        let mut workspace = test_workspace(vec![root], Default::default());
        let input = workspace.analysis_input();
        let cancel = AtomicBool::new(false);
        let computed = crate::workspace::queries::references_from_input(
            input.clone(),
            &provider_uri,
            Position::new(2, 6),
            true,
            &cancel,
        );
        assert!(computed.value.is_ok(), "reference worker must complete");
        assert!(
            !computed.records.is_empty(),
            "reference result must carry records"
        );

        let _guard = pascal_project::test_cancel_project_scan_after_checks(0);
        let error = crate::workspace::rename::revalidate_input(&input, &computed.records, &cancel)
            .expect_err("membership cancellation must abort result validation");
        assert_eq!(error, crate::workspace::rename::CANCELLATION_MESSAGE);

        let id = RequestId::from("candidate-membership-cancel".to_string());
        let mut result = AnalysisResult {
            id: AnalysisJobId::Client(AnalysisComputationId(0)),
            source_generation: computed.source_generation,
            configuration_generation: computed.configuration_generation,
            records: computed.records,
            value: AnalysisResultValue::References(computed.value),
        };
        invalidate_analysis_result(&mut result, error);

        let (server, client) = Connection::memory();
        deliver_analysis_result(&server, &mut workspace, result, Some(id.clone()))
            .expect("deliver cancellation result");
        let Message::Response(response) = client.receiver.recv().expect("cancellation response")
        else {
            panic!("expected a cancellation response");
        };
        let response_error = response.error.expect("cancellation error");
        assert_eq!(response_error.code, -32800);
        assert_eq!(
            response_error.message,
            crate::workspace::rename::CANCELLATION_MESSAGE
        );
    }

    #[test]
    fn delivery_rejects_a_stale_workspace_symbol_result() {
        let (server, client) = Connection::memory();
        let mut workspace = test_workspace(Vec::new(), Default::default());
        let id = RequestId::from("stale-workspace-symbols".to_string());
        let source_generation = workspace.source_generation();
        let configuration_generation = workspace.configuration_generation();
        deliver_analysis_result(
            &server,
            &mut workspace,
            AnalysisResult {
                id: AnalysisJobId::Client(AnalysisComputationId(0)),
                source_generation: source_generation.wrapping_add(1),
                configuration_generation,
                records: Vec::new(),
                value: AnalysisResultValue::WorkspaceSymbols(Ok(Vec::new())),
            },
            Some(id.clone()),
        )
        .expect("stale workspace symbol response");

        let Message::Response(response) = client.receiver.recv().expect("delivery response") else {
            panic!("expected a response");
        };
        assert_eq!(response.id, id);
        assert_eq!(response.error.expect("stale result error").code, -32803);
    }

    #[test]
    fn shutdown_does_not_wait_indefinitely_for_a_non_cancellable_worker() {
        let mut jobs = AnalysisJobs::new();
        let cancellation = Arc::new(AtomicBool::new(false));
        let handle = thread::spawn(|| thread::sleep(Duration::from_millis(500)));
        jobs.pending.insert(
            AnalysisComputationId(0),
            PendingAnalysis {
                cancellation: Arc::clone(&cancellation),
                handle,
                client_ids: Vec::new(),
                key: None,
            },
        );

        let started = Instant::now();
        jobs.shutdown();
        assert!(
            cancellation.load(std::sync::atomic::Ordering::Relaxed),
            "shutdown must signal every worker"
        );
        assert!(
            started.elapsed() < Duration::from_millis(250),
            "shutdown waited for a non-cancellable worker"
        );
    }

    #[test]
    fn poll_discards_a_result_without_a_pending_worker() {
        let mut jobs = AnalysisJobs::new();
        let mut workspace = test_workspace(Vec::new(), Default::default());
        jobs.sender
            .send(AnalysisResult {
                id: AnalysisJobId::Client(AnalysisComputationId(0)),
                source_generation: workspace.source_generation(),
                configuration_generation: workspace.configuration_generation(),
                records: Vec::new(),
                value: AnalysisResultValue::WorkspaceSymbols(Ok(Vec::new())),
            })
            .expect("queue orphaned result");
        let (server, client) = Connection::memory();

        jobs.poll(&server, &mut workspace)
            .expect("poll orphaned result");
        assert!(matches!(
            client.receiver.recv_timeout(Duration::from_millis(25)),
            Err(RecvTimeoutError::Timeout)
        ));
    }

    #[test]
    fn formatting_read_set_rejects_changed_configuration_without_generation_event() {
        let temp = tempfile::tempdir().expect("temporary workspace");
        let root = temp.path().to_path_buf();
        let source_path = root.join("Main.pas");
        let config_path = root.join(".fmt4d.toml");
        let source = "unit Main;\ninterface\nprocedure Run;\nimplementation\nprocedure Run;\nbegin\nend;\nend.\n";
        fs::write(&source_path, source).expect("source");
        fs::write(&config_path, "").expect("formatting configuration");

        let source_uri = Url::from_file_path(&source_path).expect("source URI");
        let config_uri = Url::from_file_path(&config_path).expect("configuration URI");
        let workspace = test_workspace(vec![root], Default::default());
        let input = workspace.analysis_input();
        let cancel = AtomicBool::new(false);
        let computed =
            crate::workspace::queries::formatting_from_input(input.clone(), &source_uri, &cancel);
        assert!(computed.value.is_ok(), "formatting worker must complete");
        assert!(
            computed
                .records
                .iter()
                .any(|record| record.uri == config_uri),
            "formatting result must retain the configuration read set"
        );

        fs::write(&config_path, "changed").expect("change formatting configuration");
        let error = crate::workspace::rename::revalidate_input(&input, &computed.records, &cancel)
            .expect_err("changed formatting configuration must invalidate the result");
        assert!(
            error.contains("configuration content changed")
                || error.contains("configuration metadata or membership changed"),
            "unexpected formatting configuration invalidation error: {error}"
        );
    }

    #[test]
    fn formatting_read_set_rejects_created_configuration_without_generation_event() {
        let temp = tempfile::tempdir().expect("temporary workspace");
        let root = temp.path().to_path_buf();
        let source_path = root.join("Main.pas");
        let config_path = root.join(".fmt4d.toml");
        let source = "unit Main;\ninterface\nprocedure Run;\nimplementation\nprocedure Run;\nbegin\nend;\nend.\n";
        fs::write(&source_path, source).expect("source");

        let source_uri = Url::from_file_path(&source_path).expect("source URI");
        let config_uri = Url::from_file_path(&config_path).expect("configuration URI");
        let workspace = test_workspace(vec![root], Default::default());
        let input = workspace.analysis_input();
        let cancel = AtomicBool::new(false);
        let computed =
            crate::workspace::queries::formatting_from_input(input.clone(), &source_uri, &cancel);
        assert!(computed.value.is_ok(), "formatting worker must complete");
        assert!(
            computed.records.iter().any(|record| {
                record.uri == config_uri
                    && record.path_stamp.is_none()
                    && record.content_bytes.is_none()
            }),
            "formatting result must retain absent configuration candidates"
        );

        fs::write(&config_path, "created").expect("create formatting configuration");
        let error = crate::workspace::rename::revalidate_input(&input, &computed.records, &cancel)
            .expect_err("created formatting configuration must invalidate the result");
        assert!(
            error.contains("configuration metadata or membership changed"),
            "unexpected formatting configuration invalidation error: {error}"
        );
    }

    #[test]
    fn delivery_rejects_completion_and_signature_after_provider_overlay_change() {
        for (kind, name) in [
            (
                AssistanceRequestKind::Completion,
                "provider-overlay-completion",
            ),
            (
                AssistanceRequestKind::SignatureHelp,
                "provider-overlay-signature",
            ),
        ] {
            let mut fixture = assistance_delivery_fixture();
            let result = start_assistance_for_delivery(&fixture, kind, name);
            match kind {
                AssistanceRequestKind::Completion => assert_completion_was_computed(&result),
                AssistanceRequestKind::SignatureHelp => assert_signature_help_was_computed(&result),
            }
            let changed_provider = fixture.provider_source.replace("Provided", "Changed");
            fixture
                .workspace
                .change_document(fixture.provider_uri.clone(), changed_provider, 2)
                .expect("change provider overlay");
            assert_stale_delivery(
                &mut fixture.workspace,
                RequestId::from(name.to_string()),
                result,
            );
        }
    }

    #[test]
    fn delivery_rejects_completion_and_signature_after_project_selection_change() {
        for (kind, name) in [
            (
                AssistanceRequestKind::Completion,
                "project-switch-completion",
            ),
            (
                AssistanceRequestKind::SignatureHelp,
                "project-switch-signature",
            ),
        ] {
            let mut fixture = assistance_delivery_fixture();
            let result = start_assistance_for_delivery(&fixture, kind, name);
            match kind {
                AssistanceRequestKind::Completion => assert_completion_was_computed(&result),
                AssistanceRequestKind::SignatureHelp => assert_signature_help_was_computed(&result),
            }
            fixture
                .workspace
                .select_project(&fixture.main_uri, Some(&fixture.project_b_uri))
                .expect("switch selected project");
            assert_stale_delivery(
                &mut fixture.workspace,
                RequestId::from(name.to_string()),
                result,
            );
        }
    }

    #[test]
    fn cancelled_empty_and_null_assistance_results_are_suppressed_before_delivery() {
        let temp = tempfile::tempdir().expect("temporary workspace");
        let root = temp.path().to_path_buf();
        let main = root.join("Main.pas");
        let source = "unit Main;\ninterface\nimplementation\nprocedure Caller;\nbegin\n  // no assistance result\nend;\nend.\n";
        fs::create_dir_all(&root).expect("fixture directory");
        fs::write(&main, source).expect("source");
        let main_uri = Url::from_file_path(&main).expect("source URI");
        let mut workspace = Workspace::new(vec![root], Default::default());
        workspace
            .open_document(main_uri.clone(), source.to_owned(), 1)
            .expect("open source overlay");
        let mut jobs = AnalysisJobs::new();

        let completion_id = RequestId::from("cancelled-empty-completion".to_string());
        jobs.start(
            completion_id.clone(),
            AnalysisRequest::Completion {
                uri: main_uri.clone(),
                position: Position::new(5, 24),
                format: MarkupKind::Markdown,
                resolve_documentation: false,
                resolve_detail: false,
            },
            &workspace,
            symbol_client_features(),
        )
        .expect("start empty completion request");
        let completion = jobs.receiver.recv().expect("computed completion result");
        assert!(matches!(
            &completion.value,
            AnalysisResultValue::Completion(CompletionAnalysis {
                value: Ok(CompletionResult { list, .. }),
                ..
            }) if list.items.is_empty()
        ));
        jobs.sender
            .send(completion)
            .expect("requeue computed completion result");
        let completion_computation_id = jobs
            .request_to_job
            .get(&completion_id)
            .copied()
            .expect("completion request mapping");
        jobs.pending
            .get(&completion_computation_id)
            .expect("pending completion")
            .cancellation
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let (server, client) = Connection::memory();
        jobs.poll(&server, &mut workspace)
            .expect("poll cancelled completion");
        let Message::Response(response) = client.receiver.recv().expect("completion response")
        else {
            panic!("expected a completion response");
        };
        assert_eq!(response.id, completion_id);
        assert_eq!(
            response.error.expect("completion cancellation").code,
            -32800
        );

        let signature_id = RequestId::from("cancelled-null-signature".to_string());
        jobs.start(
            signature_id.clone(),
            AnalysisRequest::SignatureHelp {
                uri: main_uri,
                position: Position::new(5, 24),
                format: MarkupKind::Markdown,
            },
            &workspace,
            symbol_client_features(),
        )
        .expect("start null signature request");
        let signature = jobs.receiver.recv().expect("computed signature result");
        assert!(matches!(
            &signature.value,
            AnalysisResultValue::SignatureHelp(Ok(None))
        ));
        jobs.sender
            .send(signature)
            .expect("requeue computed signature result");
        let signature_computation_id = jobs
            .request_to_job
            .get(&signature_id)
            .copied()
            .expect("signature request mapping");
        jobs.pending
            .get(&signature_computation_id)
            .expect("pending signature")
            .cancellation
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let (server, client) = Connection::memory();
        jobs.poll(&server, &mut workspace)
            .expect("poll cancelled signature");
        let Message::Response(response) = client.receiver.recv().expect("signature response")
        else {
            panic!("expected a signature response");
        };
        assert_eq!(response.id, signature_id);
        assert_eq!(response.error.expect("signature cancellation").code, -32800);
    }
}
