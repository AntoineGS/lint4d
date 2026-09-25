//! Synchronous stdio LSP protocol loop for the Pascal navigation workspace.

#[cfg(test)]
use crate::navigation::CompletionResolutionSeed;
use crate::navigation::{
    CompletionMetadata, CompletionOptions, CompletionResult, FOLDING_KIND_COMMENT,
    FOLDING_KIND_IMPORTS, FOLDING_KIND_REGION, FoldingRangeOptions, InlayHintOptions,
};
use crate::workspace::codeactions::{self, ClientActionFeatures};
use crate::workspace::queries;
use crate::workspace::rename::{self, SourceRecord};
use crate::workspace::{
    DiagnosticPublicationCursorStep, DiagnosticPublicationUriCursor, FileChange,
    MAX_CONFIGURATION_WATCH_PATHS, MAX_OPEN_DOCUMENT_URI_BYTES, NavigationState,
    PreparedWorkspaceOptions, ReconciliationBudget, RuntimeOptionsOverride, RuntimeOptionsUpdate,
    Workspace, WorkspaceOptions, canonical_file_uri, parse_runtime_options,
};
use crate::{NavigationIndex, NavigationTarget};
use crossbeam_channel::{
    Receiver, RecvTimeoutError, Sender, TryRecvError, TrySendError, bounded, unbounded,
};
use lsp_server::{Connection, ErrorCode, Message, Notification, Request, RequestId, Response};
use lsp_types::{
    ClientCapabilities, CodeAction, CodeActionOrCommand, CodeActionParams, CompletionItem,
    CompletionList, CompletionParams, CompletionResponse, ConfigurationItem, ConfigurationParams,
    DidChangeConfigurationParams, DidChangeTextDocumentParams, DidCloseTextDocumentParams,
    DidOpenTextDocumentParams, DidSaveTextDocumentParams, DocumentDiagnosticParams,
    DocumentFormattingParams, DocumentHighlightParams, FileSystemWatcher, FoldingRangeParams,
    GlobPattern, GotoDefinitionParams, GotoDefinitionResponse, HoverParams, InitializeParams,
    Location, MarkupKind, MessageType, OneOf, Position, PrepareRenameResponse, ProgressToken,
    PublishDiagnosticsParams, ReferenceParams, Registration, RegistrationParams, RelativePattern,
    SelectionRangeParams, ServerInfo, ShowMessageParams, SignatureHelpParams, SymbolInformation,
    TextDocumentIdentifier, Url, WatchKind, WorkDoneProgressCancelParams,
    WorkspaceDiagnosticParams, WorkspaceEdit, WorkspaceFolder,
};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::cell::{Cell, RefCell};
use std::collections::hash_map::RandomState;
use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::error::Error;
use std::hash::{BuildHasher, Hash, Hasher};
#[cfg(feature = "test-support")]
use std::io::Write;
use std::io::{self, BufRead, Read};
use std::mem::size_of;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
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
const OUTPUT_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);
// Bounds how long the protocol loop waits for cooperative workspace
// reconciliation before switching to conservative invalidation. This does
// not interrupt a synchronous filesystem call already in progress.
const WORKSPACE_NOTIFICATION_DEADLINE: Duration = Duration::from_secs(30);
const MAX_WATCHER_REGISTRATION_RETRIES: usize = 3;
const ANALYSIS_QUEUE_FULL_MESSAGE: &str = "analysis queue is full; retry the request";
const ANALYSIS_SUPERSEDED_MESSAGE: &str = "request superseded by a newer document version";
const OPEN_ADMISSION_FENCE_MESSAGE: &str = "analysis is disabled because an editor document could not be tracked; close the rejected document or restart the workspace";
const MAX_CONFIGURATION_DEFERRED_MESSAGES: usize = 64;
const MAX_WORKSPACE_MUTATION_DEFERRED_BYTES: usize = 1024 * 1024;
const MAX_FILE_OPERATION_BATCH_ENTRIES: usize = 64;
const MAX_FILE_OPERATION_BATCH_URI_BYTES: usize = 32 * 1024;
const MAX_FILE_OPERATION_RECOVERY_ENDPOINTS: usize = 2 * MAX_FILE_OPERATION_BATCH_ENTRIES;
const MAX_FILE_OPERATION_RECOVERY_ENDPOINT_BYTES: usize =
    MAX_FILE_OPERATION_RECOVERY_ENDPOINTS * MAX_OPEN_DOCUMENT_URI_BYTES;
// Keep one slot available for an authoritative state-changing notification
// even when only retryable feature requests are arriving.
const MAX_CONFIGURATION_DEFERRED_REQUESTS: usize =
    MAX_CONFIGURATION_DEFERRED_MESSAGES.saturating_sub(1);
const CONFIGURATION_REQUEST_RETRY_MESSAGE: &str =
    "configuration update is still being prepared; retry the request";
const DIAGNOSTIC_VALIDATION_RETRY_PREFIX: &str =
    "diagnostic validation evidence is unavailable; retry the request: ";
const MAX_COMPLETION_RESOLUTION_ENTRIES: usize = 2_048;
const MAX_COMPLETION_RESOLUTION_BYTES: usize = 8 * 1024 * 1024;
const MAX_COMPLETION_RESOLUTION_DATA_BYTES: usize = 512;
const MAX_COMPLETION_RESOLUTION_RECORDS: usize = 1_024;
const MAX_COMPLETION_RESOLUTION_CONTEXT_BYTES: usize = 2 * 1024 * 1024;
const MAX_COMPLETION_RESOLUTION_ITEM_BYTES: usize = 64 * 1024;
const COMPLETION_RESOLUTION_DATA_VERSION: u8 = 1;
const MAX_DIAGNOSTIC_RESULT_ENTRIES: usize = 2_048;
const MAX_DIAGNOSTIC_RESULT_BYTES: usize = 32 * 1024 * 1024;
const MAX_DIAGNOSTIC_DEPENDENCY_RECORDS: usize = 32_768;
// This is the mandatory worker-side freshness-evidence bound.  Optional
// result-cache admission remains smaller below, so cache pressure can only
// disable reuse; it can never discard the evidence used for validation.
const MAX_DIAGNOSTIC_VALIDATION_BYTES: usize = 64 * 1024 * 1024;
const MAX_DIAGNOSTIC_DEPENDENCY_BYTES: usize = 8 * 1024 * 1024;
const MAX_DIAGNOSTIC_RELATED_CONTRIBUTIONS: usize = MAX_DIAGNOSTIC_RESULT_ENTRIES;
const MAX_DIAGNOSTIC_REPORT_ITEMS: usize = 10_000;
const MAX_DIAGNOSTIC_REPORT_BYTES: usize = 7 * 1024 * 1024;
const MAX_DIAGNOSTIC_REPORT_ITEM_BYTES: usize = 64 * 1024;
const MAX_DIAGNOSTIC_DISPATCHES_PER_TURN: usize = 64;
const MAX_PUSH_DIAGNOSTIC_NOTIFICATIONS_PER_TURN: usize = 64;
const MAX_PUSH_DIAGNOSTIC_BYTES_PER_TURN: usize = 1024 * 1024;
const MAX_PUSH_DIAGNOSTIC_NOTIFICATION_BYTES: usize = 64 * 1024;
const LSP_FRAME_HEADER_RESERVE_BYTES: usize = 64;
// Rejected-open cleanup is bounded independently from outbound buffering.
// The workspace admits at most 10,000 open documents and each push report is
// already capped at 10,000 items; this allowance covers their deduplicated
// union plus the rejected URI.
const MAX_DIAGNOSTIC_CLEANUP_URI_BYTES_PER_TARGET: usize = 16 * 1024;
// Match the workspace's retained-publication admission ceilings. A larger
// coalesced target union indicates a broken accounting invariant; it falls
// back to stale-all rather than dropping a target.
const MAX_COALESCED_STALE_TARGETS: usize = 20_000;
const MAX_COALESCED_STALE_TARGET_URI_BYTES: usize = 16 * 1024 * 1024;
/// Progress is deliberately smaller than the analysis recipient bound.  A
/// client can attach many request recipients to one computation, but progress
/// state must not become an alternate unbounded queue.
const MAX_PROGRESS_ENTRIES: usize = 128;
const MAX_PROGRESS_CREATES: usize = 32;
const PROGRESS_CREATE_REQUEST_PREFIX: &str = "pascal-lsp-progress-create-";
const PROGRESS_TOKEN_PREFIX: &str = "pascal-lsp-progress-";
const DIAGNOSTIC_REFRESH_REQUEST_PREFIX: &str = "pascal-lsp-diagnostic-refresh-";
/// Partial results are delivered one bounded notification at a time.  Keeping
/// this separate from work-done progress prevents a large result from becoming
/// an unbounded protocol-loop operation or from being confused with lifecycle
/// progress reports.
const MAX_PARTIAL_RESULT_ITEMS_PER_CHUNK: usize = 128;
const MAX_PARTIAL_RESULT_BYTES_PER_CHUNK: usize = 64 * 1024;
const MAX_PARTIAL_RESULT_ITEM_BYTES: usize = MAX_PARTIAL_RESULT_BYTES_PER_CHUNK;
const MAX_PARTIAL_RESULT_CHUNKS_PER_TURN: usize = 1;
// A client recipient can require begin, started-report, terminal response,
// and end output while stdout is paused.  Keep enough bounded control slots
// for every admitted recipient, with a small allowance for diagnostics,
// watcher, and configuration traffic.  Data chunks have their own reserve.
const MAX_CLIENT_CONTROL_MESSAGES_PER_RECIPIENT: usize = 4;
const MAX_PENDING_OUTBOUND_CONTROL_MESSAGES: usize = MAX_CLIENT_ANALYSIS_RECIPIENTS
    * MAX_CLIENT_CONTROL_MESSAGES_PER_RECIPIENT
    + MAX_PROGRESS_ENTRIES;
const MAX_PENDING_OUTBOUND_DATA_MESSAGES: usize = 16;
const MAX_PENDING_OUTBOUND_MESSAGES: usize =
    MAX_PENDING_OUTBOUND_CONTROL_MESSAGES + MAX_PENDING_OUTBOUND_DATA_MESSAGES;
const MAX_OUTBOUND_MESSAGES: usize = 32;
/// Messages accepted by the protocol loop but not yet accepted by the
/// transport writer are retained here.  Partial-result data is deliberately
/// limited to a small prefix of this queue so terminal responses and
/// cancellation/progress control messages always have reserved capacity.
const MAX_PENDING_OUTBOUND_BYTES: usize = 16 * 1024 * 1024;
const MAX_PENDING_OUTBOUND_CONTROL_BYTES: usize = 8 * 1024 * 1024;
const MAX_PARTIAL_DELIVERY_BYTES: usize = 64 * 1024 * 1024;
// Temporary control pressure is retained separately from the normal writer
// queue.  Result payloads and lifecycle/control messages have independent
// bounded budgets so a large result cannot consume the terminal-message
// reserve.  The deferred queue is FIFO and is drained whenever writer space
// returns; it is not an unbounded retry buffer.
const MAX_DEFERRED_OUTBOUND_RESULT_BYTES: usize = MAX_PARTIAL_DELIVERY_BYTES;
const MAX_DEFERRED_OUTBOUND_CONTROL_BYTES: usize = MAX_PENDING_OUTBOUND_CONTROL_BYTES;
const MAX_DEFERRED_OUTBOUND_BYTES: usize =
    MAX_DEFERRED_OUTBOUND_RESULT_BYTES + MAX_DEFERRED_OUTBOUND_CONTROL_BYTES;
const MAX_DEFERRED_OUTBOUND_RESULT_MESSAGES: usize = MAX_PENDING_OUTBOUND_CONTROL_MESSAGES;
const MAX_DEFERRED_OUTBOUND_CONTROL_MESSAGES: usize = MAX_PENDING_OUTBOUND_CONTROL_MESSAGES;
const MAX_DEFERRED_OUTBOUND_MESSAGES: usize =
    MAX_DEFERRED_OUTBOUND_RESULT_MESSAGES + MAX_DEFERRED_OUTBOUND_CONTROL_MESSAGES;
const MAX_PARTIAL_VALIDATION_RETIREMENTS: usize = MAX_CLIENT_ANALYSIS_RECIPIENTS;

#[derive(Debug)]
enum OutputError {
    Disconnected,
    Backpressure,
    ResultBackpressure,
    MessageTooLarge,
    Encoding(String),
}

impl std::fmt::Display for OutputError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Disconnected => formatter.write_str("LSP writer disconnected"),
            Self::Backpressure => formatter
                .write_str("LSP output queue is full; the client did not drain the connection"),
            Self::ResultBackpressure => formatter.write_str(
                "deferred LSP result output is full; the client did not drain the connection",
            ),
            Self::MessageTooLarge => formatter
                .write_str("LSP output message exceeds the bounded control/output message budget"),
            Self::Encoding(error) => {
                write!(formatter, "could not encode outbound LSP message: {error}")
            }
        }
    }
}

impl Error for OutputError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OutboundClass {
    Data,
    Control,
    Result,
}

impl OutboundClass {
    fn is_control(self) -> bool {
        matches!(self, Self::Control | Self::Result)
    }

    fn is_result(self) -> bool {
        matches!(self, Self::Result)
    }
}

#[derive(Debug)]
struct PendingOutboundMessage {
    message: Message,
    bytes: usize,
    class: OutboundClass,
    diagnostic_uri: Option<Url>,
    diagnostic_generation: Option<u64>,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct DiagnosticPublicationDiscardScan {
    scanned_messages: usize,
    scanned_bytes: usize,
    removed_messages: usize,
}

#[derive(Debug, Default)]
struct OutboundQueue {
    pending: VecDeque<PendingOutboundMessage>,
    pending_bytes: usize,
    pending_data_messages: usize,
    pending_control_messages: usize,
    pending_control_bytes: usize,
    deferred: VecDeque<PendingOutboundMessage>,
    deferred_bytes: usize,
    deferred_control_messages: usize,
    deferred_result_messages: usize,
    deferred_control_bytes: usize,
    deferred_result_bytes: usize,
    #[cfg(feature = "test-support")]
    test_control_message_limit: Option<usize>,
    next_diagnostic_publication_generation: u64,
}

impl OutboundQueue {
    fn discard_diagnostic_publications(
        &mut self,
        uris: &std::collections::BTreeSet<Url>,
    ) -> DiagnosticPublicationDiscardScan {
        self.discard_diagnostic_publications_matching(Some(uris))
    }

    fn discard_all_diagnostic_publications(&mut self) -> DiagnosticPublicationDiscardScan {
        self.discard_diagnostic_publications_matching(None)
    }

    fn discard_diagnostic_publications_matching(
        &mut self,
        uris: Option<&std::collections::BTreeSet<Url>>,
    ) -> DiagnosticPublicationDiscardScan {
        let stale_before = self.next_diagnostic_publication_generation;
        self.next_diagnostic_publication_generation = self
            .next_diagnostic_publication_generation
            .saturating_add(1);
        let mut scan = DiagnosticPublicationDiscardScan::default();

        let mut pending_bytes = 0usize;
        let mut pending_data_messages = 0usize;
        let mut pending_control_messages = 0usize;
        let mut pending_control_bytes = 0usize;
        self.pending.retain(|pending| {
            scan.scanned_messages = scan.scanned_messages.saturating_add(1);
            scan.scanned_bytes = scan.scanned_bytes.saturating_add(pending.bytes);
            let superseded = pending.diagnostic_uri.as_ref().is_some_and(|uri| {
                (match uris {
                    Some(uris) => uris.contains(uri),
                    None => true,
                }) && pending
                    .diagnostic_generation
                    .is_some_and(|generation| generation < stale_before)
            });
            if superseded {
                scan.removed_messages = scan.removed_messages.saturating_add(1);
                return false;
            }
            pending_bytes = pending_bytes.saturating_add(pending.bytes);
            match pending.class {
                OutboundClass::Data => pending_data_messages += 1,
                OutboundClass::Control | OutboundClass::Result => {
                    pending_control_messages += 1;
                    pending_control_bytes = pending_control_bytes.saturating_add(pending.bytes);
                }
            }
            true
        });
        self.pending_bytes = pending_bytes;
        self.pending_data_messages = pending_data_messages;
        self.pending_control_messages = pending_control_messages;
        self.pending_control_bytes = pending_control_bytes;

        let mut deferred_bytes = 0usize;
        let mut deferred_control_messages = 0usize;
        let mut deferred_result_messages = 0usize;
        let mut deferred_control_bytes = 0usize;
        let mut deferred_result_bytes = 0usize;
        self.deferred.retain(|pending| {
            scan.scanned_messages = scan.scanned_messages.saturating_add(1);
            scan.scanned_bytes = scan.scanned_bytes.saturating_add(pending.bytes);
            let superseded = pending.diagnostic_uri.as_ref().is_some_and(|uri| {
                (match uris {
                    Some(uris) => uris.contains(uri),
                    None => true,
                }) && pending
                    .diagnostic_generation
                    .is_some_and(|generation| generation < stale_before)
            });
            if superseded {
                scan.removed_messages = scan.removed_messages.saturating_add(1);
                return false;
            }
            deferred_bytes = deferred_bytes.saturating_add(pending.bytes);
            if pending.class.is_result() {
                deferred_result_messages += 1;
                deferred_result_bytes = deferred_result_bytes.saturating_add(pending.bytes);
            } else {
                deferred_control_messages += 1;
                deferred_control_bytes = deferred_control_bytes.saturating_add(pending.bytes);
            }
            true
        });
        self.deferred_bytes = deferred_bytes;
        self.deferred_control_messages = deferred_control_messages;
        self.deferred_result_messages = deferred_result_messages;
        self.deferred_control_bytes = deferred_control_bytes;
        self.deferred_result_bytes = deferred_result_bytes;
        scan
    }

    fn control_message_limit(&self, pending: &PendingOutboundMessage) -> usize {
        #[cfg(not(feature = "test-support"))]
        let _ = pending;
        #[cfg(feature = "test-support")]
        if matches!(pending.class, OutboundClass::Control)
            && matches!(
                &pending.message,
                Message::Notification(notification)
                    if notification.method == "textDocument/publishDiagnostics"
            )
        {
            if let Some(limit) = self.test_control_message_limit {
                return limit;
            }
        }
        MAX_PENDING_OUTBOUND_CONTROL_MESSAGES
    }

    fn has_pending(&self) -> bool {
        !self.pending.is_empty() || !self.deferred.is_empty()
    }

    fn flush(&mut self, sender: &Sender<Message>) -> Result<(), OutputError> {
        loop {
            while let Some(pending) = self.pending.front() {
                match sender.try_send(pending.message.clone()) {
                    Ok(()) => {
                        let pending = self.pending.pop_front().expect("pending message exists");
                        self.pending_bytes = self.pending_bytes.saturating_sub(pending.bytes);
                        match pending.class {
                            OutboundClass::Data => {
                                self.pending_data_messages =
                                    self.pending_data_messages.saturating_sub(1);
                            }
                            OutboundClass::Control | OutboundClass::Result => {
                                self.pending_control_messages =
                                    self.pending_control_messages.saturating_sub(1);
                                self.pending_control_bytes =
                                    self.pending_control_bytes.saturating_sub(pending.bytes);
                            }
                        }
                    }
                    Err(TrySendError::Full(_)) => return Ok(()),
                    Err(TrySendError::Disconnected(_)) => return Err(OutputError::Disconnected),
                }
            }

            let Some(deferred) = self.deferred.front() else {
                return Ok(());
            };
            if !self.can_fit_pending(deferred) {
                return Ok(());
            }
            let deferred = self.deferred.pop_front().expect("deferred message exists");
            self.deferred_bytes = self.deferred_bytes.saturating_sub(deferred.bytes);
            if deferred.class.is_result() {
                self.deferred_result_messages = self.deferred_result_messages.saturating_sub(1);
                self.deferred_result_bytes =
                    self.deferred_result_bytes.saturating_sub(deferred.bytes);
            } else {
                self.deferred_control_messages = self.deferred_control_messages.saturating_sub(1);
                self.deferred_control_bytes =
                    self.deferred_control_bytes.saturating_sub(deferred.bytes);
            }
            self.push_pending(deferred);
        }
    }

    fn can_fit_pending(&self, pending: &PendingOutboundMessage) -> bool {
        let bytes = pending.bytes;
        let class = pending.class;
        let total_messages = self
            .pending_data_messages
            .saturating_add(self.pending_control_messages);
        if total_messages >= MAX_PENDING_OUTBOUND_MESSAGES
            || self.pending_bytes.saturating_add(bytes) > MAX_PENDING_OUTBOUND_BYTES
        {
            return false;
        }
        match class {
            OutboundClass::Data => self.pending_data_messages < MAX_PENDING_OUTBOUND_DATA_MESSAGES,
            OutboundClass::Control | OutboundClass::Result => {
                self.pending_control_messages < self.control_message_limit(pending)
                    && self.pending_control_bytes.saturating_add(bytes)
                        <= MAX_PENDING_OUTBOUND_CONTROL_BYTES
            }
        }
    }

    fn push_pending(&mut self, pending: PendingOutboundMessage) {
        self.pending_bytes = self.pending_bytes.saturating_add(pending.bytes);
        match pending.class {
            OutboundClass::Data => self.pending_data_messages += 1,
            OutboundClass::Control | OutboundClass::Result => {
                self.pending_control_messages += 1;
                self.pending_control_bytes =
                    self.pending_control_bytes.saturating_add(pending.bytes);
            }
        }
        self.pending.push_back(pending);
    }

    fn defer(&mut self, pending: PendingOutboundMessage) -> Result<bool, OutputError> {
        // A successful deferral is output admission: the message is now
        // owned by this bounded queue, so the producer may retire its
        // request/result state without retrying or duplicating it.  Results
        // and lifecycle controls have separate count/byte budgets so a
        // result burst cannot consume all control capacity.
        let control_limit = self.control_message_limit(&pending);
        let (deferred_messages, max_deferred_messages, deferred_bytes, max_deferred_bytes) =
            if pending.class.is_result() {
                (
                    &mut self.deferred_result_messages,
                    MAX_DEFERRED_OUTBOUND_RESULT_MESSAGES,
                    &mut self.deferred_result_bytes,
                    MAX_DEFERRED_OUTBOUND_RESULT_BYTES,
                )
            } else {
                (
                    &mut self.deferred_control_messages,
                    control_limit,
                    &mut self.deferred_control_bytes,
                    MAX_DEFERRED_OUTBOUND_CONTROL_BYTES,
                )
            };
        if *deferred_messages >= max_deferred_messages
            || deferred_bytes.saturating_add(pending.bytes) > max_deferred_bytes
            || self.deferred.len() >= MAX_DEFERRED_OUTBOUND_MESSAGES
            || self.deferred_bytes.saturating_add(pending.bytes) > MAX_DEFERRED_OUTBOUND_BYTES
        {
            return Err(if pending.class.is_result() {
                OutputError::ResultBackpressure
            } else {
                OutputError::Backpressure
            });
        }
        *deferred_messages += 1;
        *deferred_bytes = deferred_bytes.saturating_add(pending.bytes);
        self.deferred_bytes = self.deferred_bytes.saturating_add(pending.bytes);
        self.deferred.push_back(pending);
        Ok(true)
    }

    fn enqueue(
        &mut self,
        sender: &Sender<Message>,
        message: Message,
        class: OutboundClass,
    ) -> Result<bool, OutputError> {
        self.flush(sender)?;
        let bytes = serde_json::to_vec(&message)
            .map_err(|error| OutputError::Encoding(error.to_string()))?
            .len();
        if bytes > MAX_PENDING_OUTBOUND_BYTES
            || (class.is_control() && bytes > MAX_PENDING_OUTBOUND_CONTROL_BYTES)
        {
            return Err(OutputError::MessageTooLarge);
        }
        let pending = PendingOutboundMessage {
            diagnostic_uri: diagnostic_publication_uri(&message),
            diagnostic_generation: None,
            message,
            bytes,
            class,
        };
        let mut pending = pending;
        if pending.diagnostic_uri.is_some() {
            pending.diagnostic_generation = Some(self.next_diagnostic_publication_generation);
            self.next_diagnostic_publication_generation = self
                .next_diagnostic_publication_generation
                .saturating_add(1);
        }
        if !self.deferred.is_empty() {
            return match class {
                OutboundClass::Data => Ok(false),
                OutboundClass::Control | OutboundClass::Result => self.defer(pending),
            };
        }
        if !self.can_fit_pending(&pending) {
            return match class {
                OutboundClass::Data => Ok(false),
                OutboundClass::Control | OutboundClass::Result => self.defer(pending),
            };
        }
        self.push_pending(pending);
        self.flush(sender)?;
        Ok(true)
    }

    #[cfg(test)]
    fn deferred_result_bytes(&self) -> usize {
        self.deferred_result_bytes
    }
}

/// Protocol output is accepted without blocking the event loop.  The
/// production wrapper retains a bounded, ordered queue in front of the
/// transport channel; the plain `Connection` implementation keeps the
/// memory-connection unit tests lightweight.
trait ProtocolSender {
    fn send_control(&self, message: Message) -> Result<(), OutputError>;
    fn send_result(&self, message: Message) -> Result<(), OutputError>;
    fn send_data(&self, message: Message) -> Result<bool, OutputError>;
    fn discard_diagnostic_publications(
        &self,
        _uris: &std::collections::BTreeSet<Url>,
    ) -> DiagnosticPublicationDiscardScan {
        DiagnosticPublicationDiscardScan::default()
    }
    fn discard_all_diagnostic_publications(&self) -> DiagnosticPublicationDiscardScan {
        DiagnosticPublicationDiscardScan::default()
    }
}

/// Collects diagnostic staling requests and applies one queue filter for a
/// notification or analysis-completion turn. Diagnostic sends can optionally
/// be held until the caller has flushed staling, preserving cleanup ordering.
struct DiagnosticPublicationBatchSender<'a> {
    inner: &'a dyn ProtocolSender,
    targets: RefCell<BTreeSet<Url>>,
    target_uri_bytes: Cell<usize>,
    stale_all: Cell<bool>,
    staling_requested: Cell<bool>,
    defer_diagnostic_sends: bool,
    deferred_progress: RefCell<Vec<Message>>,
    deferred_progress_bytes: Cell<usize>,
}

impl<'a> DiagnosticPublicationBatchSender<'a> {
    fn new(inner: &'a dyn ProtocolSender, defer_diagnostic_sends: bool) -> Self {
        Self {
            inner,
            targets: RefCell::new(BTreeSet::new()),
            target_uri_bytes: Cell::new(0),
            stale_all: Cell::new(false),
            staling_requested: Cell::new(false),
            defer_diagnostic_sends,
            deferred_progress: RefCell::new(Vec::new()),
            deferred_progress_bytes: Cell::new(0),
        }
    }

    fn add_targets(&self, targets: &BTreeSet<Url>) {
        if self.stale_all.get() || targets.is_empty() {
            return;
        }
        let mut accumulated = self.targets.borrow_mut();
        for target in targets {
            if accumulated.contains(target) {
                continue;
            }
            let Some(bytes) = self
                .target_uri_bytes
                .get()
                .checked_add(target.as_str().len())
                .filter(|bytes| *bytes <= MAX_COALESCED_STALE_TARGET_URI_BYTES)
            else {
                self.stale_all.set(true);
                accumulated.clear();
                self.target_uri_bytes.set(0);
                return;
            };
            if accumulated.len() >= MAX_COALESCED_STALE_TARGETS {
                self.stale_all.set(true);
                accumulated.clear();
                self.target_uri_bytes.set(0);
                return;
            }
            accumulated.insert(target.clone());
            self.target_uri_bytes.set(bytes);
        }
    }

    fn mark_all_stale(&self) {
        self.stale_all.set(true);
        self.targets.borrow_mut().clear();
        self.target_uri_bytes.set(0);
    }

    fn targets(&self) -> BTreeSet<Url> {
        self.targets.borrow().clone()
    }

    fn is_stale_all(&self) -> bool {
        self.stale_all.get()
    }

    fn staling_requested(&self) -> bool {
        self.staling_requested.get() || self.stale_all.get()
    }

    fn has_staling_work(&self) -> bool {
        self.stale_all.get() || !self.targets.borrow().is_empty()
    }

    fn flush_deferred_progress(&self) -> Result<(), OutputError> {
        for message in std::mem::take(&mut *self.deferred_progress.borrow_mut()) {
            self.inner.send_control(message)?;
        }
        self.deferred_progress_bytes.set(0);
        Ok(())
    }

    fn flush(&self) -> DiagnosticPublicationDiscardScan {
        if self.stale_all.get() {
            self.inner.discard_all_diagnostic_publications()
        } else if self.targets.borrow().is_empty() {
            DiagnosticPublicationDiscardScan::default()
        } else {
            self.inner
                .discard_diagnostic_publications(&self.targets.borrow())
        }
    }
}

impl ProtocolSender for DiagnosticPublicationBatchSender<'_> {
    fn send_control(&self, message: Message) -> Result<(), OutputError> {
        if self.defer_diagnostic_sends && diagnostic_publication_uri(&message).is_some() {
            return Err(OutputError::Backpressure);
        }
        if self.defer_diagnostic_sends
            && matches!(
                &message,
                Message::Notification(notification) if notification.method == "$/progress"
            )
        {
            let bytes = serde_json::to_vec(&message)
                .map_err(|error| OutputError::Encoding(error.to_string()))?
                .len();
            let next_bytes = self.deferred_progress_bytes.get().saturating_add(bytes);
            if self.deferred_progress.borrow().len() >= MAX_PENDING_OUTBOUND_CONTROL_MESSAGES
                || next_bytes > MAX_PENDING_OUTBOUND_CONTROL_BYTES
            {
                return Err(OutputError::Backpressure);
            }
            self.deferred_progress.borrow_mut().push(message);
            self.deferred_progress_bytes.set(next_bytes);
            return Ok(());
        }
        self.inner.send_control(message)
    }

    fn send_result(&self, message: Message) -> Result<(), OutputError> {
        self.inner.send_result(message)
    }

    fn send_data(&self, message: Message) -> Result<bool, OutputError> {
        self.inner.send_data(message)
    }

    fn discard_diagnostic_publications(
        &self,
        uris: &BTreeSet<Url>,
    ) -> DiagnosticPublicationDiscardScan {
        self.staling_requested.set(true);
        self.add_targets(uris);
        DiagnosticPublicationDiscardScan::default()
    }

    fn discard_all_diagnostic_publications(&self) -> DiagnosticPublicationDiscardScan {
        self.staling_requested.set(true);
        self.mark_all_stale();
        DiagnosticPublicationDiscardScan::default()
    }
}

impl ProtocolSender for Connection {
    fn send_control(&self, message: Message) -> Result<(), OutputError> {
        match self.sender.try_send(message) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => Err(OutputError::Backpressure),
            Err(TrySendError::Disconnected(_)) => Err(OutputError::Disconnected),
        }
    }

    fn send_result(&self, message: Message) -> Result<(), OutputError> {
        self.send_control(message)
    }

    fn send_data(&self, message: Message) -> Result<bool, OutputError> {
        match self.sender.try_send(message) {
            Ok(()) => Ok(true),
            Err(TrySendError::Full(_)) => Ok(false),
            Err(TrySendError::Disconnected(_)) => Err(OutputError::Disconnected),
        }
    }
}

struct ProtocolConnection {
    connection: Connection,
    outbound: RefCell<OutboundQueue>,
    priority_receiver: Option<Receiver<Message>>,
    #[cfg(feature = "test-support")]
    workspace_fifo_probe: Option<WorkspaceFifoProbePaths>,
}

impl ProtocolConnection {
    fn new(
        connection: Connection,
        priority_receiver: Receiver<Message>,
        test_barriers: &TestBarrierConfig,
    ) -> Self {
        let outbound = OutboundQueue::default();
        #[cfg(feature = "test-support")]
        let outbound = {
            let mut outbound = outbound;
            outbound.test_control_message_limit = test_barriers.outbound_control_limit;
            outbound
        };
        #[cfg(not(feature = "test-support"))]
        let _ = test_barriers;
        Self {
            connection,
            outbound: RefCell::new(outbound),
            priority_receiver: Some(priority_receiver),
            #[cfg(feature = "test-support")]
            workspace_fifo_probe: test_barriers.workspace_fifo_probe.clone(),
        }
    }

    #[cfg(feature = "test-support")]
    fn record_workspace_fifo_state(
        &self,
        queued_count: usize,
        queued_bytes: usize,
        overflow: Option<&Message>,
    ) {
        let Some(probe) = self.workspace_fifo_probe.as_ref() else {
            return;
        };
        let overflow_bytes = overflow
            .and_then(|message| serde_json::to_vec(message).ok())
            .map_or(0, |bytes| bytes.len());
        let worker_held = probe.worker_entered.exists() && !probe.worker_release.exists();
        let snapshot = serde_json::json!({
            "queued_count": queued_count,
            "queued_bytes": queued_bytes,
            "overflow_occupied": overflow.is_some(),
            "overflow_bytes": overflow_bytes,
            "full": queued_count >= MAX_CONFIGURATION_DEFERRED_MESSAGES && overflow.is_some(),
            "reader_pending": false,
            "worker_held": worker_held,
        });
        let _ = std::fs::write(&probe.state, snapshot.to_string());
    }

    fn initialize_start(&self) -> Result<(RequestId, Value), lsp_server::ProtocolError> {
        self.connection.initialize_start()
    }

    fn initialize_finish(
        &self,
        initialize_id: RequestId,
        initialize_result: Value,
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        self.send_control(Message::Response(Response::new_ok(
            initialize_id,
            initialize_result,
        )))
        .map_err(|error| -> Box<dyn Error + Send + Sync> { error.into() })?;
        match self.receiver().recv() {
            Ok(Message::Notification(notification)) if notification.method == "initialized" => {
                Ok(())
            }
            Ok(message) => {
                Err(format!("expected initialized notification, got: {message:?}").into())
            }
            Err(_) => Err("LSP client disconnected during initialization".into()),
        }
    }

    fn receiver(&self) -> &Receiver<Message> {
        &self.connection.receiver
    }

    fn try_recv_priority(&self) -> Option<Message> {
        self.priority_receiver
            .as_ref()
            .and_then(|receiver| receiver.try_recv().ok())
    }

    fn flush(&self) -> Result<(), OutputError> {
        self.outbound.borrow_mut().flush(&self.connection.sender)
    }

    fn has_pending_output(&self) -> bool {
        self.outbound.borrow().has_pending()
    }

    fn drain_until(&self, deadline: Instant) -> Result<(), OutputError> {
        while self.has_pending_output() {
            self.flush()?;
            if !self.has_pending_output() || Instant::now() >= deadline {
                break;
            }
            thread::sleep(ANALYSIS_POLL_INTERVAL);
        }
        self.flush()
    }
}

impl ProtocolSender for ProtocolConnection {
    fn send_control(&self, message: Message) -> Result<(), OutputError> {
        self.outbound.borrow_mut().enqueue(
            &self.connection.sender,
            message,
            OutboundClass::Control,
        )?;
        Ok(())
    }

    fn send_result(&self, message: Message) -> Result<(), OutputError> {
        self.outbound.borrow_mut().enqueue(
            &self.connection.sender,
            message,
            OutboundClass::Result,
        )?;
        Ok(())
    }

    fn send_data(&self, message: Message) -> Result<bool, OutputError> {
        self.outbound
            .borrow_mut()
            .enqueue(&self.connection.sender, message, OutboundClass::Data)
    }

    fn discard_diagnostic_publications(
        &self,
        uris: &std::collections::BTreeSet<Url>,
    ) -> DiagnosticPublicationDiscardScan {
        self.outbound
            .borrow_mut()
            .discard_diagnostic_publications(uris)
    }

    fn discard_all_diagnostic_publications(&self) -> DiagnosticPublicationDiscardScan {
        self.outbound
            .borrow_mut()
            .discard_all_diagnostic_publications()
    }
}

struct UnusedProtocolSender;

impl ProtocolSender for UnusedProtocolSender {
    fn send_control(&self, _message: Message) -> Result<(), OutputError> {
        Err(OutputError::Disconnected)
    }

    fn send_result(&self, _message: Message) -> Result<(), OutputError> {
        Err(OutputError::Disconnected)
    }

    fn send_data(&self, _message: Message) -> Result<bool, OutputError> {
        Err(OutputError::Disconnected)
    }
}

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
            | AnalysisRequest::PrepareCallHierarchy { .. }
            | AnalysisRequest::PrepareTypeHierarchy { .. }
            | AnalysisRequest::TypeHierarchySupertypes { .. }
            | AnalysisRequest::TypeHierarchySubtypes { .. }
            | AnalysisRequest::IncomingCalls { .. }
            | AnalysisRequest::OutgoingCalls { .. }
            | AnalysisRequest::CodeActions(_)
            | AnalysisRequest::Resolve(_)
            | AnalysisRequest::ResolveCompletion(_)
            | AnalysisRequest::DocumentHighlights { .. }
            | AnalysisRequest::SelectionRanges { .. } => Self::Interactive,
            AnalysisRequest::DocumentLinks { .. } => Self::Bulk,
            AnalysisRequest::Diagnostics { .. }
            | AnalysisRequest::DocumentDiagnostics { .. }
            | AnalysisRequest::WorkspaceDiagnostics { .. } => Self::Diagnostics,
            AnalysisRequest::Formatting { .. }
            | AnalysisRequest::DocumentSymbols { .. }
            | AnalysisRequest::WorkspaceSymbols { .. }
            | AnalysisRequest::References { .. }
            | AnalysisRequest::Rename { .. }
            | AnalysisRequest::SemanticTokens { .. }
            | AnalysisRequest::FoldingRanges { .. }
            | AnalysisRequest::InlayHints { .. } => Self::Bulk,
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
    WorkspaceSymbols,
    References,
    PartialValidation,
}

#[cfg(feature = "test-support")]
#[derive(Clone, Debug, Default)]
pub struct TestBarrierConfig {
    navigation: Option<TestBarrierPaths>,
    formatting: Option<TestBarrierPaths>,
    diagnostics: Option<TestBarrierPaths>,
    selection: Option<TestBarrierPaths>,
    completion_resolution: Option<TestBarrierPaths>,
    workspace_symbols: Option<TestBarrierPaths>,
    references: Option<TestBarrierPaths>,
    partial_validation: Option<TestBarrierPaths>,
    outbound_writer: Option<OutboundWriterBarrierPaths>,
    outbound_control_limit: Option<usize>,
    workspace_fifo_probe: Option<WorkspaceFifoProbePaths>,
    dispatch: Option<PathBuf>,
}

#[cfg(feature = "test-support")]
#[derive(Clone, Debug)]
struct TestBarrierPaths {
    entered: PathBuf,
    release: PathBuf,
}

#[cfg(feature = "test-support")]
#[derive(Clone, Debug)]
struct OutboundWriterBarrierPaths {
    armed: PathBuf,
    entered: PathBuf,
    release: PathBuf,
}

#[cfg(feature = "test-support")]
#[derive(Clone, Debug)]
struct WorkspaceFifoProbePaths {
    state: PathBuf,
    reader_pending: PathBuf,
    worker_entered: PathBuf,
    worker_release: PathBuf,
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
            workspace_symbols: None,
            references: None,
            partial_validation: None,
            outbound_writer: None,
            outbound_control_limit: None,
            workspace_fifo_probe: None,
            dispatch: None,
        }
    }

    pub fn with_outbound_writer(
        mut self,
        outbound_writer: Option<(PathBuf, PathBuf, PathBuf, usize)>,
    ) -> Self {
        self.outbound_writer = outbound_writer.map(|(armed, entered, release, control_limit)| {
            self.outbound_control_limit = Some(control_limit);
            OutboundWriterBarrierPaths {
                armed,
                entered,
                release,
            }
        });
        self
    }

    pub fn with_workspace_fifo_probe(
        mut self,
        probe: Option<(PathBuf, PathBuf, PathBuf, PathBuf)>,
    ) -> Self {
        self.workspace_fifo_probe =
            probe.map(|(state, reader_pending, worker_entered, worker_release)| {
                WorkspaceFifoProbePaths {
                    state,
                    reader_pending,
                    worker_entered,
                    worker_release,
                }
            });
        self
    }

    fn record_workspace_fifo_reader_pending(&self, message: &Message) {
        let Some(probe) = self.workspace_fifo_probe.as_ref() else {
            return;
        };
        let frame_bytes = serde_json::to_vec(message).map_or(0, |bytes| bytes.len());
        let method = match message {
            Message::Request(request) => request.method.as_str(),
            Message::Notification(notification) => notification.method.as_str(),
            Message::Response(_) => "<response>",
        };
        let mut snapshot = std::fs::read(&probe.state)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            .unwrap_or_else(|| serde_json::json!({}));
        let worker_held = probe.worker_entered.exists() && !probe.worker_release.exists();
        if snapshot["full"] != true || !worker_held {
            return;
        }
        if let Some(object) = snapshot.as_object_mut() {
            object.insert("reader_pending".to_string(), Value::Bool(true));
            object.insert(
                "reader_pending_method".to_string(),
                Value::String(method.to_string()),
            );
            object.insert(
                "reader_pending_bytes".to_string(),
                Value::from(frame_bytes as u64),
            );
            object.insert("worker_held".to_string(), Value::Bool(worker_held));
        }
        let _ = std::fs::write(&probe.state, snapshot.to_string());
        let _ = std::fs::write(
            &probe.reader_pending,
            serde_json::json!({
                "reader_pending": true,
                "method": method,
                "frame_bytes": frame_bytes,
                "worker_held": worker_held,
            })
            .to_string(),
        );
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

    pub fn with_workspace_symbols(mut self, workspace_symbols: Option<(PathBuf, PathBuf)>) -> Self {
        self.workspace_symbols =
            workspace_symbols.map(|(entered, release)| TestBarrierPaths { entered, release });
        self
    }

    pub fn with_references(mut self, references: Option<(PathBuf, PathBuf)>) -> Self {
        self.references =
            references.map(|(entered, release)| TestBarrierPaths { entered, release });
        self
    }

    pub fn with_partial_validation(
        mut self,
        partial_validation: Option<(PathBuf, PathBuf)>,
    ) -> Self {
        self.partial_validation =
            partial_validation.map(|(entered, release)| TestBarrierPaths { entered, release });
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
            TestBarrier::WorkspaceSymbols => self.workspace_symbols.as_ref(),
            TestBarrier::References => self.references.as_ref(),
            TestBarrier::PartialValidation => self.partial_validation.as_ref(),
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

#[cfg(feature = "test-support")]
fn wait_at_uninterruptible_test_barrier(
    barrier: TestBarrier,
    config: &TestBarrierConfig,
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
    while !paths.release.exists() {
        thread::sleep(ANALYSIS_POLL_INTERVAL);
    }
    Ok(())
}

#[cfg(not(feature = "test-support"))]
fn wait_at_uninterruptible_test_barrier(
    _barrier: TestBarrier,
    _config: &TestBarrierConfig,
) -> Result<(), String> {
    Ok(())
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
    rename_file: bool,
    will_rename_files: bool,
    hierarchical_document_symbols: bool,
    hover_format: DocumentationFormat,
    completion_format: DocumentationFormat,
    completion_snippet_support: bool,
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
    snippet_support: bool,
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
    snippet_support: bool,
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
            snippet_support,
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
            snippet_support,
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
                // The compact observation below intentionally drops the full
                // overlay text.  Revalidation therefore interprets this as a
                // raw-byte hash, while parsed_text_hash retains the decoded
                // LSP-text identity for parser freshness.
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
            candidate_observations: record.candidate_observations.clone(),
            read_policy: record.read_policy.clone(),
            path_entry: record.path_entry.clone(),
            include_payload: record.include_payload,
            missing_provider_candidate: record.missing_provider_candidate,
            document_link_missing_candidate: record.document_link_missing_candidate,
            directory_observation: record.directory_observation,
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
        snippet_support: bool,
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
        range: Option<lsp_types::Range>,
        on_type_cursor: Option<Position>,
        tab_size: u32,
        insert_spaces: bool,
    },
    DocumentLinks {
        uri: Url,
    },
    Diagnostics {
        uri: Url,
    },
    DocumentDiagnostics {
        uri: Url,
        previous_result_id: Option<String>,
        related_document_support: bool,
    },
    WorkspaceDiagnostics {
        previous_result_ids: Vec<(Url, String)>,
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
        new_uri: Option<Url>,
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
    InlayHints {
        uri: Url,
        range: lsp_types::Range,
    },
    PrepareCallHierarchy {
        uri: Url,
        position: Position,
    },
    IncomingCalls {
        item: lsp_types::CallHierarchyItem,
    },
    OutgoingCalls {
        item: lsp_types::CallHierarchyItem,
    },
    PrepareTypeHierarchy {
        uri: Url,
        position: Position,
    },
    TypeHierarchySupertypes {
        item: lsp_types::TypeHierarchyItem,
    },
    TypeHierarchySubtypes {
        item: lsp_types::TypeHierarchyItem,
    },
}

fn progress_title(request: &AnalysisRequest) -> &'static str {
    match request {
        AnalysisRequest::Diagnostics { .. } => "Indexing workspace",
        AnalysisRequest::DocumentDiagnostics { .. } => "Indexing document diagnostics",
        AnalysisRequest::WorkspaceDiagnostics { .. } => "Indexing workspace diagnostics",
        AnalysisRequest::WorkspaceSymbols { .. } => "Searching workspace symbols",
        AnalysisRequest::References { .. } => "Searching workspace references",
        AnalysisRequest::Rename { .. } => "Preparing rename",
        AnalysisRequest::CodeActions(_) | AnalysisRequest::Resolve(_) => "Preparing code actions",
        AnalysisRequest::Formatting { .. } => "Formatting document",
        AnalysisRequest::DocumentLinks { .. } => "Resolving document links",
        AnalysisRequest::DocumentSymbols { .. } => "Indexing document symbols",
        AnalysisRequest::SemanticTokens { .. } => "Computing semantic tokens",
        AnalysisRequest::FoldingRanges { .. } => "Computing folding ranges",
        AnalysisRequest::InlayHints { .. } => "Computing inlay hints",
        AnalysisRequest::PrepareCallHierarchy { .. } => "Preparing call hierarchy",
        AnalysisRequest::PrepareTypeHierarchy { .. } => "Preparing type hierarchy",
        AnalysisRequest::TypeHierarchySupertypes { .. } => "Resolving type supertypes",
        AnalysisRequest::TypeHierarchySubtypes { .. } => "Searching type subtypes",
        AnalysisRequest::IncomingCalls { .. } => "Searching incoming calls",
        AnalysisRequest::OutgoingCalls { .. } => "Searching outgoing calls",
        AnalysisRequest::Hover { .. }
        | AnalysisRequest::Completion { .. }
        | AnalysisRequest::SignatureHelp { .. }
        | AnalysisRequest::Navigation { .. }
        | AnalysisRequest::TypeDefinitions { .. }
        | AnalysisRequest::Prepare { .. }
        | AnalysisRequest::ResolveCompletion(_)
        | AnalysisRequest::DocumentHighlights { .. }
        | AnalysisRequest::SelectionRanges { .. } => "Analyzing document",
    }
}

#[derive(Clone)]
enum AnalysisResultValue {
    Hover(Result<Option<lsp_types::Hover>, String>),
    Completion(CompletionAnalysis),
    ResolveCompletion(CompletionResolutionAnalysis),
    SignatureHelp(Result<Option<lsp_types::SignatureHelp>, String>),
    Navigation(NavigationAnalysis),
    Formatting(Result<Vec<lsp_types::TextEdit>, String>),
    DocumentLinks(Result<Vec<lsp_types::DocumentLink>, String>),
    Diagnostics(DiagnosticsAnalysis),
    DocumentDiagnostics(Result<DocumentDiagnosticsAnalysis, String>),
    WorkspaceDiagnostics(Result<WorkspaceDiagnosticsAnalysis, String>),
    TypeDefinitions(Result<Vec<lsp_types::Location>, String>),
    Prepare(Result<PrepareRenameResponse, String>),
    Rename {
        value: Box<Result<WorkspaceEdit, String>>,
        unit_file_move: Option<(Url, Url)>,
    },
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
    InlayHints(Result<Vec<lsp_types::InlayHint>, String>),
    PrepareCallHierarchy(Result<Option<Vec<lsp_types::CallHierarchyItem>>, String>),
    IncomingCalls(Result<Vec<lsp_types::CallHierarchyIncomingCall>, String>),
    OutgoingCalls(Result<Vec<lsp_types::CallHierarchyOutgoingCall>, String>),
    PrepareTypeHierarchy(Result<Option<Vec<lsp_types::TypeHierarchyItem>>, String>),
    TypeHierarchySupertypes(Result<Option<Vec<lsp_types::TypeHierarchyItem>>, String>),
    TypeHierarchySubtypes(Result<Option<Vec<lsp_types::TypeHierarchyItem>>, String>),
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
    value: Result<Vec<queries::DiagnosticPublication>, String>,
    discard: bool,
}

#[derive(Clone)]
struct DocumentDiagnosticsAnalysis {
    uri: Url,
    previous_result_id: Option<String>,
    related_document_support: bool,
    publications: Vec<queries::DiagnosticPublication>,
    dependencies: HashMap<Url, DiagnosticDependency>,
    invalid_related_owners: Vec<Url>,
}

#[derive(Clone)]
struct WorkspaceDiagnosticsAnalysis {
    previous_result_ids: Vec<(Url, String)>,
    publications: Vec<queries::DiagnosticPublication>,
    dependencies: HashMap<Url, DiagnosticDependency>,
}

/// Worker-prepared evidence for one effective diagnostic report.  The full
/// source records are compacted before delivery; the protocol thread only
/// compares this bounded fingerprint and retained-size charge.
#[derive(Clone)]
struct DiagnosticDependency {
    identity: Arc<Vec<u8>>,
    records: Arc<Vec<SourceRecord>>,
    source_generation: u64,
    configuration_generation: u64,
    retained_bytes: usize,
}

fn diagnostic_dependency_retained_bytes(identity: &[u8], records: &[SourceRecord]) -> usize {
    size_of::<DiagnosticDependency>()
        .saturating_add(identity.len())
        .saturating_add(source_records_retained_bytes(records))
}

fn diagnostic_validation_retry(error: String) -> String {
    format!("{DIAGNOSTIC_VALIDATION_RETRY_PREFIX}{error}")
}

fn diagnostic_items_retained_bytes(diagnostics: &[lsp_types::Diagnostic]) -> Result<usize, String> {
    let encoded = serde_json::to_vec(diagnostics)
        .map_err(|error| format!("could not encode diagnostic storage estimate: {error}"))?;
    Ok(size_of::<Vec<lsp_types::Diagnostic>>().saturating_add(encoded.len()))
}

fn compact_diagnostic_records(records: &[SourceRecord]) -> Result<Vec<SourceRecord>, String> {
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
        let content_hash = record.content_hash.or_else(|| {
            if record.open {
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
            return Err(format!(
                "diagnostic dependency {} has no bounded content observation",
                record.uri
            ));
        }
        let compact_record = SourceRecord {
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
            directory_observation: record.directory_observation,
            missing_provider_scope: record.missing_provider_scope.clone(),
            auto_import_provider_observation: record.auto_import_provider_observation,
            auto_import_scopes: record.auto_import_scopes.clone(),
        };
        compact.push(compact_record);
    }
    Ok(compact)
}

fn diagnostic_record_fingerprint(record: &SourceRecord) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::fmt::Debug;

    fn hash_debug<T: Debug>(hasher: &mut DefaultHasher, value: &T) {
        format!("{value:?}").hash(hasher);
    }

    let mut hasher = DefaultHasher::new();
    record.uri.hash(&mut hasher);
    record.open.hash(&mut hasher);
    record.content_hash.hash(&mut hasher);
    record.parsed_text_hash.hash(&mut hasher);
    record.include_payload.hash(&mut hasher);
    record.missing_provider_candidate.hash(&mut hasher);
    record.document_link_missing_candidate.hash(&mut hasher);
    record.directory_observation.hash(&mut hasher);
    record.auto_import_provider_observation.hash(&mut hasher);
    hash_debug(&mut hasher, &record.candidate_membership);
    hash_debug(&mut hasher, &record.candidate_observations);
    hash_debug(&mut hasher, &record.read_policy);
    hash_debug(&mut hasher, &record.path_entry);
    hash_debug(&mut hasher, &record.missing_provider_scope);
    hash_debug(&mut hasher, &record.auto_import_scopes);
    hasher.finish()
}

fn diagnostic_records_identity(records: &[SourceRecord]) -> Vec<u8> {
    let mut ordered = records.iter().collect::<Vec<_>>();
    ordered.sort_by(|left, right| {
        left.uri.as_str().cmp(right.uri.as_str()).then_with(|| {
            diagnostic_record_fingerprint(left).cmp(&diagnostic_record_fingerprint(right))
        })
    });
    let mut identity = Vec::with_capacity(ordered.len().saturating_mul(32));
    for record in ordered {
        let uri = record.uri.as_str().as_bytes();
        identity.extend_from_slice(&(uri.len() as u64).to_le_bytes());
        identity.extend_from_slice(uri);
        identity.extend_from_slice(&diagnostic_record_fingerprint(record).to_le_bytes());
    }
    identity
}

fn prepare_diagnostic_dependency(
    records: &[SourceRecord],
    source_generation: u64,
    configuration_generation: u64,
) -> Result<Option<DiagnosticDependency>, String> {
    if records.is_empty() {
        return Ok(None);
    }
    let records = Arc::new(compact_diagnostic_records(records)?);
    let identity = diagnostic_records_identity(&records);
    let retained_bytes = diagnostic_dependency_retained_bytes(&identity, &records);
    Ok(Some(DiagnosticDependency {
        retained_bytes,
        identity: Arc::new(identity),
        records,
        source_generation,
        configuration_generation,
    }))
}

fn prepare_diagnostic_dependencies_with_generations(
    dependencies: &HashMap<Url, Arc<Vec<SourceRecord>>>,
    source_generation: u64,
    configuration_generation: u64,
) -> Result<HashMap<Url, DiagnosticDependency>, String> {
    let mut prepared = HashMap::<usize, Option<DiagnosticDependency>>::new();
    let mut result = HashMap::with_capacity(dependencies.len());
    for (uri, records) in dependencies {
        let key = Arc::as_ptr(records) as usize;
        let dependency = if let Some(dependency) = prepared.get(&key) {
            dependency.clone()
        } else {
            let dependency = prepare_diagnostic_dependency(
                records.as_slice(),
                source_generation,
                configuration_generation,
            )?;
            prepared.insert(key, dependency.clone());
            dependency
        };
        if let Some(dependency) = dependency {
            result.insert(uri.clone(), dependency);
        }
    }
    Ok(result)
}

fn validate_related_owner_evidence(
    input: &rename::WorkspaceInput,
    owners: &[RelatedOwnerValidation],
    cancel: &AtomicBool,
) -> Result<Vec<Url>, String> {
    let mut validated = HashMap::<usize, bool>::new();
    let mut invalid = Vec::new();
    for owner in owners {
        let owner_valid = !owner.dependencies.is_empty()
            && owner.dependencies.iter().all(|dependency| {
                if dependency.records.is_empty()
                    || dependency.configuration_generation != input.configuration_generation
                {
                    return false;
                }
                let key = Arc::as_ptr(&dependency.records) as usize;
                if let Some(valid) = validated.get(&key) {
                    return *valid;
                }
                let valid = match rename::revalidate_effective_input(
                    input,
                    dependency.records.as_slice(),
                    cancel,
                ) {
                    Ok(()) => true,
                    Err(error) if error == rename::CANCELLATION_MESSAGE => {
                        return false;
                    }
                    Err(_) => false,
                };
                validated.insert(key, valid);
                valid
            });
        if !owner_valid {
            invalid.push(owner.root_uri.clone());
        }
        if cancel.load(std::sync::atomic::Ordering::Relaxed) {
            return Err(rename::CANCELLATION_MESSAGE.to_string());
        }
    }
    invalid.sort_by(|left, right| left.as_str().cmp(right.as_str()));
    invalid.dedup();
    Ok(invalid)
}

#[derive(Clone)]
struct DiagnosticResultCacheEntry {
    result_id: String,
    uri: Url,
    version: Option<i32>,
    diagnostics: Vec<lsp_types::Diagnostic>,
    records_identity: Vec<u8>,
    diagnostics_identity: Vec<u8>,
    retained_bytes: usize,
    cacheable: bool,
    dependency: Option<DiagnosticDependency>,
}

#[derive(Default)]
struct DiagnosticPullStore {
    entries: HashMap<String, DiagnosticResultCacheEntry>,
    uri_to_result: HashMap<Url, String>,
    related_owners: HashMap<Url, RelatedDiagnosticOwner>,
    related_owner_order: VecDeque<Url>,
    order: VecDeque<String>,
    retained_bytes: usize,
    next_id: u64,
}

#[derive(Default)]
struct DiagnosticPublicationTurnBudget {
    notification_count: usize,
    processed_targets: usize,
    serialized_bytes: usize,
    stale_root_visits: usize,
    stale_target_visits: usize,
    stale_root_uri_bytes: usize,
    stale_target_uri_bytes: usize,
    publication_queue_scans: usize,
    publication_queue_messages_scanned: usize,
    publication_queue_bytes_scanned: usize,
}

#[derive(Clone)]
struct RelatedDiagnosticContribution {
    diagnostics: Vec<lsp_types::Diagnostic>,
    dependency: Option<DiagnosticDependency>,
}

#[derive(Clone)]
struct RelatedDiagnosticOwner {
    contributions: HashMap<Url, RelatedDiagnosticContribution>,
    retained_bytes: usize,
}

#[derive(Clone)]
struct RelatedOwnerValidation {
    root_uri: Url,
    dependencies: Vec<DiagnosticDependency>,
}

#[derive(Default)]
struct RelatedOwnerReconciliation {
    invalidated_roots: HashSet<Url>,
    affected: HashSet<Url>,
}

struct RelatedOwnerTransaction {
    root_uri: Url,
    removed_roots: HashSet<Url>,
    replacement: Option<RelatedDiagnosticOwner>,
    reports: Vec<RelatedDiagnosticReport>,
}

struct RelatedDiagnosticReport {
    uri: Url,
    diagnostics: Vec<lsp_types::Diagnostic>,
    dependency: Option<DiagnosticDependency>,
}

impl DiagnosticPullStore {
    fn new() -> Self {
        Self::default()
    }

    fn prepare_related_owner(
        &self,
        root_uri: &Url,
        publications: impl IntoIterator<
            Item = (
                Url,
                Vec<lsp_types::Diagnostic>,
                Option<DiagnosticDependency>,
            ),
        >,
        reconciliation: RelatedOwnerReconciliation,
    ) -> Result<RelatedOwnerTransaction, String> {
        let publications = publications.into_iter().collect::<Vec<_>>();
        if publications.len() > MAX_DIAGNOSTIC_REPORT_ITEMS {
            return Err(
                "related diagnostic contribution count exceeds the bounded limit".to_string(),
            );
        }

        let mut current = HashMap::with_capacity(publications.len());
        for (uri, diagnostics, dependency) in publications {
            if !diagnostics.is_empty() && dependency.is_none() {
                return Err(format!(
                    "related diagnostic {uri} has no bounded freshness evidence"
                ));
            }
            current.insert(
                uri,
                RelatedDiagnosticContribution {
                    diagnostics,
                    dependency,
                },
            );
        }
        let current_owner = if current.is_empty() {
            None
        } else {
            let retained_bytes = self.related_owner_retained_bytes(root_uri, &current)?;
            Some(RelatedDiagnosticOwner {
                contributions: current,
                retained_bytes,
            })
        };
        let current_bytes = current_owner
            .as_ref()
            .map_or(0, |owner| owner.retained_bytes);
        let current_contributions = current_owner
            .as_ref()
            .map_or(0, |owner| owner.contributions.len());
        let mut removed_roots = reconciliation.invalidated_roots;
        removed_roots.insert(root_uri.clone());
        let mut removed_bytes = 0usize;
        let mut removed_contributions = 0usize;
        let mut removed_owner_count = 0usize;
        let mut affected = reconciliation.affected;
        for removed_root in &removed_roots {
            let Some(owner) = self.related_owners.get(removed_root) else {
                continue;
            };
            removed_owner_count = removed_owner_count.saturating_add(1);
            removed_bytes = removed_bytes.saturating_add(owner.retained_bytes);
            removed_contributions = removed_contributions.saturating_add(owner.contributions.len());
            affected.extend(owner.contributions.keys().cloned());
        }
        affected.extend(
            current_owner
                .as_ref()
                .into_iter()
                .flat_map(|owner| owner.contributions.keys().cloned()),
        );
        let candidate_owner_count = self
            .related_owners
            .len()
            .saturating_sub(removed_owner_count)
            .saturating_add(usize::from(current_owner.is_some()));
        if candidate_owner_count > MAX_DIAGNOSTIC_RESULT_ENTRIES {
            return Err("related diagnostic owner capacity is full; retry the request".to_string());
        }
        let retained_contributions = self
            .related_owners
            .values()
            .map(|owner| owner.contributions.len())
            .sum::<usize>();
        if retained_contributions
            .saturating_sub(removed_contributions)
            .saturating_add(current_contributions)
            > MAX_DIAGNOSTIC_RELATED_CONTRIBUTIONS
        {
            return Err(
                "related diagnostic contribution capacity is full; retry the request".to_string(),
            );
        }
        let candidate_total = self
            .retained_bytes
            .saturating_sub(removed_bytes)
            .saturating_add(current_bytes);
        if candidate_total > MAX_DIAGNOSTIC_RESULT_BYTES {
            return Err(
                "related diagnostic ownership exceeds the bounded result-state budget; retry the request"
                    .to_string(),
            );
        }
        let reports = self.related_reports_for_candidate(
            affected,
            root_uri,
            current_owner.as_ref(),
            &removed_roots,
        )?;
        Ok(RelatedOwnerTransaction {
            root_uri: root_uri.clone(),
            removed_roots,
            replacement: current_owner,
            reports,
        })
    }

    fn commit_related_owner(&mut self, transaction: RelatedOwnerTransaction) {
        for root_uri in &transaction.removed_roots {
            if let Some(owner) = self.related_owners.remove(root_uri) {
                self.retained_bytes = self.retained_bytes.saturating_sub(owner.retained_bytes);
            }
            self.related_owner_order.retain(|uri| uri != root_uri);
        }
        if let Some(owner) = transaction.replacement {
            self.retained_bytes = self.retained_bytes.saturating_add(owner.retained_bytes);
            self.related_owner_order
                .push_back(transaction.root_uri.clone());
            self.related_owners.insert(transaction.root_uri, owner);
        }
    }

    fn related_owner_retained_bytes(
        &self,
        root_uri: &Url,
        contributions: &HashMap<Url, RelatedDiagnosticContribution>,
    ) -> Result<usize, String> {
        if contributions.len() > MAX_DIAGNOSTIC_RELATED_CONTRIBUTIONS {
            return Err(
                "related diagnostic contribution count exceeds the bounded limit".to_string(),
            );
        }
        let mut bytes = size_of::<RelatedDiagnosticOwner>()
            .saturating_add(size_of::<Url>())
            .saturating_add(root_uri.as_str().len())
            .saturating_add(
                contributions
                    .capacity()
                    .saturating_mul(size_of::<(Url, RelatedDiagnosticContribution)>()),
            );
        for (uri, contribution) in contributions {
            bytes = bytes
                .saturating_add(size_of::<Url>())
                .saturating_add(uri.as_str().len())
                .saturating_add(size_of::<RelatedDiagnosticContribution>())
                .saturating_add(diagnostic_items_retained_bytes(&contribution.diagnostics)?);
            if let Some(dependency) = &contribution.dependency {
                bytes = bytes.saturating_add(dependency.retained_bytes);
            }
        }
        Ok(bytes)
    }

    fn related_owner_validation_snapshot(&self) -> Vec<RelatedOwnerValidation> {
        self.related_owners
            .iter()
            .map(|(root_uri, owner)| RelatedOwnerValidation {
                root_uri: root_uri.clone(),
                dependencies: owner
                    .contributions
                    .values()
                    .filter_map(|contribution| contribution.dependency.clone())
                    .collect(),
            })
            .collect()
    }

    fn reconcile_related_owners(
        &self,
        invalidated: impl IntoIterator<Item = Url>,
    ) -> RelatedOwnerReconciliation {
        let invalidated = invalidated.into_iter().collect::<HashSet<_>>();
        let mut affected = HashSet::new();
        for root_uri in &invalidated {
            if let Some(owner) = self.related_owners.get(root_uri) {
                affected.extend(owner.contributions.keys().cloned());
            }
        }
        RelatedOwnerReconciliation {
            invalidated_roots: invalidated,
            affected,
        }
    }

    fn related_reports_for_candidate(
        &self,
        affected: HashSet<Url>,
        replacement_root: &Url,
        replacement: Option<&RelatedDiagnosticOwner>,
        removed_roots: &HashSet<Url>,
    ) -> Result<Vec<RelatedDiagnosticReport>, String> {
        let mut affected = affected.into_iter().collect::<Vec<_>>();
        affected.sort_by(|left, right| left.as_str().cmp(right.as_str()));
        affected
            .into_iter()
            .map(|uri| {
                let mut diagnostics = Vec::new();
                let mut dependency_identity = Vec::new();
                let mut dependency_records = Vec::new();
                let mut dependency_records_available = true;
                let mut dependency_records_bytes = 0usize;
                let mut source_generation: Option<u64> = None;
                let mut configuration_generation: Option<u64> = None;
                let mut owners = self
                    .related_owners
                    .iter()
                    .filter(|(owner_uri, _)| {
                        *owner_uri != replacement_root && !removed_roots.contains(*owner_uri)
                    })
                    .collect::<Vec<_>>();
                if let Some(replacement) = replacement {
                    owners.push((replacement_root, replacement));
                }
                owners.sort_by(|left, right| left.0.as_str().cmp(right.0.as_str()));
                for (owner_uri, owner) in owners {
                    let Some(contribution) = owner.contributions.get(&uri) else {
                        continue;
                    };
                    for diagnostic in &contribution.diagnostics {
                        if !diagnostics.contains(diagnostic) {
                            diagnostics.push(diagnostic.clone());
                        }
                    }
                    let Some(dependency) = contribution.dependency.as_ref() else {
                        continue;
                    };
                    let owner_uri = owner_uri.as_str().as_bytes();
                    dependency_identity.extend_from_slice(&(owner_uri.len() as u64).to_le_bytes());
                    dependency_identity.extend_from_slice(owner_uri);
                    dependency_identity
                        .extend_from_slice(&(dependency.identity.len() as u64).to_le_bytes());
                    dependency_identity.extend_from_slice(dependency.identity.as_slice());
                    if dependency_records_available {
                        let record_bytes = source_records_retained_bytes(&dependency.records);
                        if dependency_records_bytes.saturating_add(record_bytes)
                            <= MAX_DIAGNOSTIC_DEPENDENCY_BYTES
                        {
                            dependency_records.extend(dependency.records.iter().cloned());
                            dependency_records_bytes =
                                dependency_records_bytes.saturating_add(record_bytes);
                        } else {
                            dependency_records.clear();
                            dependency_records_available = false;
                        }
                    }
                    source_generation = Some(
                        source_generation.map_or(dependency.source_generation, |generation| {
                            generation.min(dependency.source_generation)
                        }),
                    );
                    configuration_generation = Some(
                        configuration_generation
                            .map_or(dependency.configuration_generation, |generation| {
                                generation.min(dependency.configuration_generation)
                            }),
                    );
                }
                if diagnostics.len() > MAX_DIAGNOSTIC_REPORT_ITEMS {
                    return Err("related diagnostic report item limit reached".to_string());
                }
                let diagnostics_bytes = diagnostic_items_retained_bytes(&diagnostics)?;
                if diagnostics_bytes > MAX_DIAGNOSTIC_REPORT_BYTES {
                    return Err("related diagnostic report byte limit reached".to_string());
                }
                let dependency = (!dependency_identity.is_empty() && dependency_records_available)
                    .then(|| {
                        let records = Arc::new(dependency_records);
                        let retained_bytes =
                            diagnostic_dependency_retained_bytes(&dependency_identity, &records);
                        DiagnosticDependency {
                            identity: Arc::new(dependency_identity),
                            records,
                            source_generation: source_generation.unwrap_or_default(),
                            configuration_generation: configuration_generation.unwrap_or_default(),
                            retained_bytes,
                        }
                    });
                Ok(RelatedDiagnosticReport {
                    uri,
                    diagnostics,
                    dependency,
                })
            })
            .collect()
    }

    fn identity_for(
        diagnostics: &[lsp_types::Diagnostic],
        records_identity: &[u8],
    ) -> Result<(Vec<u8>, Vec<u8>), String> {
        let diagnostics_identity = serde_json::to_vec(diagnostics)
            .map_err(|error| format!("could not encode diagnostic result identity: {error}"))?;
        Ok((diagnostics_identity, records_identity.to_vec()))
    }

    fn remove(&mut self, result_id: &str) -> Option<DiagnosticResultCacheEntry> {
        let entry = self.entries.remove(result_id)?;
        self.retained_bytes = self.retained_bytes.saturating_sub(entry.retained_bytes);
        if self
            .uri_to_result
            .get(&entry.uri)
            .is_some_and(|current| current == result_id)
        {
            self.uri_to_result.remove(&entry.uri);
        }
        self.order.retain(|id| id != result_id);
        Some(entry)
    }

    fn evict_if_needed(&mut self) {
        while self.entries.len() > MAX_DIAGNOSTIC_RESULT_ENTRIES
            || self.retained_bytes > MAX_DIAGNOSTIC_RESULT_BYTES
        {
            let Some(result_id) = self.order.pop_front() else {
                break;
            };
            let _ = self.remove(&result_id);
        }
    }

    fn preview_insert(
        &self,
        uri: Url,
        version: Option<i32>,
        diagnostics: Vec<lsp_types::Diagnostic>,
        dependency: Option<&DiagnosticDependency>,
        next_id: &mut u64,
    ) -> Result<DiagnosticResultCacheEntry, String> {
        let dependency_identity =
            dependency.map_or(&[][..], |dependency| dependency.identity.as_slice());
        let (diagnostics_identity, records_identity) =
            Self::identity_for(&diagnostics, dependency_identity)?;
        let previous = self
            .uri_to_result
            .get(&uri)
            .cloned()
            .and_then(|result_id| self.entries.get(&result_id))
            .cloned();
        let result_id = previous
            .as_ref()
            .filter(|previous| {
                previous.cacheable
                    && previous.dependency.is_some()
                    && dependency.is_some()
                    && previous.diagnostics_identity == diagnostics_identity
                    && previous.records_identity == records_identity
            })
            .map(|previous| previous.result_id.clone())
            .unwrap_or_else(|| {
                *next_id = next_id.wrapping_add(1);
                format!("{SERVER_NAME}-diagnostic-{next_id}")
            });
        let retained_bytes = size_of::<DiagnosticResultCacheEntry>()
            .saturating_add(uri.as_str().len())
            .saturating_add(result_id.len())
            .saturating_add(diagnostics_identity.len())
            .saturating_add(records_identity.len())
            .saturating_add(dependency.map_or(0, |dependency| dependency.retained_bytes));
        let retained_bytes = retained_bytes.saturating_add(
            size_of::<Vec<lsp_types::Diagnostic>>().saturating_add(diagnostics_identity.len()),
        );
        let cacheable = dependency.is_some_and(|dependency| {
            dependency.retained_bytes <= MAX_DIAGNOSTIC_DEPENDENCY_BYTES
                && retained_bytes <= MAX_DIAGNOSTIC_RESULT_BYTES
        });
        Ok(DiagnosticResultCacheEntry {
            result_id: result_id.clone(),
            uri: uri.clone(),
            version,
            diagnostics,
            diagnostics_identity,
            records_identity,
            retained_bytes,
            cacheable,
            dependency: dependency.cloned(),
        })
    }

    fn commit_entries(
        &mut self,
        entries: impl IntoIterator<Item = DiagnosticResultCacheEntry>,
        next_id: u64,
    ) {
        for entry in entries {
            if let Some(previous_id) = self.uri_to_result.get(&entry.uri).cloned() {
                let _ = self.remove(&previous_id);
            }
            if !entry.cacheable {
                continue;
            }
            self.retained_bytes = self.retained_bytes.saturating_add(entry.retained_bytes);
            self.uri_to_result
                .insert(entry.uri.clone(), entry.result_id.clone());
            self.order.push_back(entry.result_id.clone());
            self.entries.insert(entry.result_id.clone(), entry);
        }
        self.next_id = next_id;
        self.evict_if_needed();
    }

    fn insert(
        &mut self,
        uri: Url,
        version: Option<i32>,
        diagnostics: Vec<lsp_types::Diagnostic>,
        dependency: Option<&DiagnosticDependency>,
    ) -> Result<DiagnosticResultCacheEntry, String> {
        let mut next_id = self.next_id;
        let entry = self.preview_insert(uri, version, diagnostics, dependency, &mut next_id)?;
        self.commit_entries(std::iter::once(entry.clone()), next_id);
        Ok(entry)
    }

    fn get(&self, result_id: &str) -> Option<&DiagnosticResultCacheEntry> {
        self.entries.get(result_id)
    }
}

#[derive(Debug, Default)]
struct DiagnosticNotificationEffect {
    refresh: Vec<Url>,
    cancel: Vec<Url>,
    stale_publication_targets: BTreeSet<Url>,
    clear_publication_cursor: Option<DiagnosticPublicationUriCursor>,
    cleanup_rejected_uri: Option<Url>,
    refresh_membership: HashSet<Url>,
    cancel_membership: HashSet<Url>,
    refresh_requested: bool,
    refresh_all_diagnostics: bool,
    discard_all_queued_diagnostics: bool,
}

#[derive(Debug, Default)]
struct PendingDiagnosticClears {
    targets: VecDeque<DiagnosticClearTarget>,
    cursor: Option<DiagnosticPublicationUriCursor>,
}

#[derive(Debug)]
struct DiagnosticClearTarget {
    uri: Url,
    version: Option<i32>,
}

impl PendingDiagnosticClears {
    fn enqueue_cursor(
        &mut self,
        cursor: DiagnosticPublicationUriCursor,
        rejected_uri: Option<Url>,
    ) {
        if let Some(pending) = self.cursor.as_mut() {
            if let Some(uri) = rejected_uri {
                pending.add_late_target(uri);
            }
        } else {
            self.cursor = Some(cursor);
        }
    }

    fn is_empty(&self) -> bool {
        self.targets.is_empty() && self.cursor.is_none()
    }

    fn fill_batch(&mut self, step_limit: usize) {
        let mut cursor_steps = 0usize;
        while cursor_steps < step_limit {
            let Some(cursor) = self.cursor.as_mut() else {
                break;
            };
            cursor_steps += 1;
            match cursor.next_step() {
                DiagnosticPublicationCursorStep::Target(uri) => {
                    if uri.as_str().len() <= MAX_DIAGNOSTIC_CLEANUP_URI_BYTES_PER_TARGET {
                        self.targets
                            .push_back(DiagnosticClearTarget { uri, version: None });
                    }
                }
                DiagnosticPublicationCursorStep::Skipped => {}
                DiagnosticPublicationCursorStep::Exhausted => {
                    self.cursor = None;
                    break;
                }
            }
        }
    }

    fn pump(
        &mut self,
        connection: &dyn ProtocolSender,
        budget: &mut DiagnosticPublicationTurnBudget,
    ) -> Result<(), OutputError> {
        let remaining = MAX_DIAGNOSTIC_DISPATCHES_PER_TURN
            .saturating_sub(budget.processed_targets)
            .min(MAX_DIAGNOSTIC_DISPATCHES_PER_TURN.saturating_sub(budget.notification_count));
        if self.targets.is_empty() {
            self.fill_batch(remaining);
        }
        for _ in 0..remaining {
            let Some(target) = self.targets.front() else {
                break;
            };
            let message = diagnostics_notification(&target.uri, target.version, Vec::new());
            let bytes = serde_json::to_vec(&message)
                .map_err(|error| OutputError::Encoding(error.to_string()))?
                .len()
                .saturating_add(LSP_FRAME_HEADER_RESERVE_BYTES);
            if bytes > MAX_PUSH_DIAGNOSTIC_NOTIFICATION_BYTES {
                return Err(OutputError::MessageTooLarge);
            }
            if budget.serialized_bytes.saturating_add(bytes) > MAX_PUSH_DIAGNOSTIC_BYTES_PER_TURN {
                break;
            }
            match send_diagnostic_clear(connection, &target.uri, target.version) {
                Ok(()) => {
                    self.targets.pop_front().expect("front cleanup target");
                    budget.notification_count += 1;
                    budget.processed_targets += 1;
                    budget.serialized_bytes = budget.serialized_bytes.saturating_add(bytes);
                }
                Err(OutputError::Backpressure) => return Ok(()),
                Err(error) => return Err(error),
            }
        }
        if self.is_empty() {
            self.cursor = None;
        }
        Ok(())
    }
}

impl DiagnosticNotificationEffect {
    fn request_refresh(&mut self) {
        self.refresh_requested = true;
    }

    fn refresh_uri(&mut self, uri: Url) {
        if self.refresh_membership.insert(uri.clone()) {
            self.refresh.push(uri);
        }
    }

    fn refresh_all_diagnostics(&mut self) {
        self.refresh_all_diagnostics = true;
        self.refresh_requested = true;
    }

    fn cancel_uri(&mut self, uri: Url) {
        if self.cancel_membership.insert(uri.clone()) {
            self.cancel.push(uri);
        }
    }

    fn refresh_uri_with_budget(
        &mut self,
        uri: Url,
        budget: Option<&ReconciliationBudget>,
    ) -> Result<(), String> {
        if !self.refresh_membership.contains(&uri) {
            if let Some(budget) = budget {
                budget.charge_diagnostic_target(&uri)?;
            }
            self.refresh_membership.insert(uri.clone());
            self.refresh.push(uri);
        }
        Ok(())
    }

    fn cancel_uri_with_budget(
        &mut self,
        uri: Url,
        budget: Option<&ReconciliationBudget>,
    ) -> Result<(), String> {
        if !self.cancel_membership.contains(&uri) {
            if let Some(budget) = budget {
                budget.charge_diagnostic_target(&uri)?;
            }
            self.cancel_membership.insert(uri.clone());
            self.cancel.push(uri);
        }
        Ok(())
    }

    fn refresh_dependents(
        &mut self,
        workspace: &Workspace,
        changed_uri: &Url,
        include_parent: bool,
    ) {
        for uri in workspace.diagnostic_dependents_for_change(changed_uri, include_parent) {
            self.refresh_uri(uri);
        }
    }

    fn refresh_dependents_with_control(
        &mut self,
        workspace: &Workspace,
        changed_uri: &Url,
        include_parent: bool,
        cancel: Option<&AtomicBool>,
        budget: Option<&ReconciliationBudget>,
    ) -> Result<(), String> {
        let Some(budget) = budget else {
            self.refresh_dependents(workspace, changed_uri, include_parent);
            return Ok(());
        };
        #[cfg(feature = "test-support")]
        wait_at_diagnostic_work_test_barrier(budget)?;
        workspace.visit_diagnostic_dependents_for_change(
            changed_uri,
            include_parent,
            cancel,
            budget,
            |uri| self.refresh_uri_with_budget(uri.clone(), Some(budget)),
        )
    }
}

#[derive(Debug, Default)]
struct DiagnosticRefreshRequests {
    supported: bool,
    pending: bool,
    in_flight: Option<RequestId>,
    retired: VecDeque<RequestId>,
    next_id: u64,
}

impl DiagnosticRefreshRequests {
    fn new(supported: bool) -> Self {
        Self {
            supported,
            ..Self::default()
        }
    }

    fn send(
        &mut self,
        connection: &dyn ProtocolSender,
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        self.next_id = self.next_id.wrapping_add(1);
        let id = RequestId::from(format!(
            "{DIAGNOSTIC_REFRESH_REQUEST_PREFIX}{}",
            self.next_id
        ));
        connection.send_control(Message::Request(Request::new(
            id.clone(),
            "workspace/diagnostic/refresh".to_string(),
            Value::Null,
        )))?;
        self.in_flight = Some(id);
        Ok(())
    }

    fn request(
        &mut self,
        connection: &dyn ProtocolSender,
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        if !self.supported {
            return Ok(());
        }
        if self.in_flight.is_some() {
            self.pending = true;
            return Ok(());
        }
        self.send(connection)
    }

    fn handle_response(
        &mut self,
        connection: &dyn ProtocolSender,
        response: &Response,
    ) -> Result<bool, Box<dyn Error + Send + Sync>> {
        if self.in_flight.as_ref() == Some(&response.id) {
            self.in_flight = None;
            self.retired.push_back(response.id.clone());
            if self.retired.len() > 64 {
                self.retired.pop_front();
            }
            if self.pending {
                self.pending = false;
                self.send(connection)?;
            }
            return Ok(true);
        }
        // Late and duplicate responses are harmless. Consume only IDs that
        // this coordinator actually issued, leaving unrelated client replies
        // for the normal routing paths.
        Ok(self.retired.iter().any(|id| id == &response.id))
    }

    fn shutdown(&mut self) {
        self.supported = false;
        self.pending = false;
        if let Some(id) = self.in_flight.take() {
            self.retired.push_back(id);
        }
        while self.retired.len() > 64 {
            self.retired.pop_front();
        }
    }
}

const RUNTIME_CONFIGURATION_SECTION: &str = "pascalLsp";
const CONFIGURATION_REQUEST_PREFIX: &str = "pascal-lsp-configuration-";
const MAX_RETIRED_CONFIGURATION_REQUESTS: usize = 8;

#[derive(Debug, Clone, PartialEq, Eq)]
enum ConfigurationScope {
    Global,
    Scoped(Url),
}

impl ConfigurationScope {
    fn from_uri(uri: Option<Url>) -> Self {
        uri.map_or(Self::Global, Self::Scoped)
    }

    fn is_compatible_with(&self, scope_uri: Option<&Url>) -> bool {
        match self {
            Self::Global => true,
            Self::Scoped(scope) => scope_uri.is_some_and(|current| current == scope),
        }
    }
}

struct ConfigurationPreparationRequest {
    revision: u64,
    options: WorkspaceOptions,
    root_paths: Vec<PathBuf>,
}

struct ConfigurationPreparationResult {
    revision: u64,
    options: WorkspaceOptions,
    root_paths: Vec<PathBuf>,
    prepared: Result<PreparedWorkspaceOptions, String>,
}

#[derive(Debug, Clone)]
struct DeferredDocumentIdentity {
    uri: Url,
    version: Option<i32>,
    generation: Option<u64>,
}

#[derive(Debug)]
struct DeferredConfigurationRequest {
    request: Request,
    configuration_revision: u64,
    document: Option<DeferredDocumentIdentity>,
    preceding_document_notification: bool,
    preceding_configuration_notification: bool,
}

#[derive(Debug)]
enum DeferredConfigurationMessage {
    Request(DeferredConfigurationRequest),
    Notification(Notification),
}

struct PendingConfigurationPreparation {
    cancellation: Arc<AtomicBool>,
    handle: JoinHandle<()>,
}

struct ConfigurationPreparationJobs {
    sender: Sender<ConfigurationPreparationResult>,
    receiver: Receiver<ConfigurationPreparationResult>,
    pending: Option<PendingConfigurationPreparation>,
    queued: Option<ConfigurationPreparationRequest>,
    shutting_down: bool,
}

impl ConfigurationPreparationJobs {
    fn new() -> Self {
        let (sender, receiver) = crossbeam_channel::bounded(1);
        Self {
            sender,
            receiver,
            pending: None,
            queued: None,
            shutting_down: false,
        }
    }

    fn request(&mut self, request: ConfigurationPreparationRequest) -> Result<(), String> {
        if self.shutting_down {
            return Err("configuration preparation is shutting down".to_string());
        }
        if let Some(pending) = &self.pending {
            pending
                .cancellation
                .store(true, std::sync::atomic::Ordering::Relaxed);
            self.queued = Some(request);
            return Ok(());
        }
        self.spawn(request)
    }

    fn spawn(&mut self, request: ConfigurationPreparationRequest) -> Result<(), String> {
        let cancellation = Arc::new(AtomicBool::new(false));
        let worker_cancellation = Arc::clone(&cancellation);
        let sender = self.sender.clone();
        let revision = request.revision;
        let options = request.options;
        let root_paths = request.root_paths;
        let worker_options = options.clone();
        let worker_root_paths = root_paths.clone();
        let handle = thread::Builder::new()
            .name("PascalLspConfiguration".to_string())
            .spawn(move || {
                let prepared = Workspace::prepare_runtime_options_for_roots(
                    worker_root_paths.clone(),
                    worker_options,
                    &worker_cancellation,
                );
                let _ = sender.send(ConfigurationPreparationResult {
                    revision,
                    options,
                    root_paths: worker_root_paths,
                    prepared,
                });
            })
            .map_err(|error| format!("could not start configuration worker: {error}"))?;
        self.pending = Some(PendingConfigurationPreparation {
            cancellation,
            handle,
        });
        Ok(())
    }

    fn cancel_pending(&mut self) {
        self.queued = None;
        if let Some(pending) = &self.pending {
            pending
                .cancellation
                .store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }

    fn poll(&mut self) -> Option<ConfigurationPreparationResult> {
        let result = self.receiver.try_recv().ok()?;
        if let Some(pending) = self.pending.take() {
            let _ = pending.handle.join();
        }
        if let Some(request) = self.queued.take() {
            if let Err(error) = self.spawn(request) {
                eprintln!("pascal-lsp: configuration worker failed to start: {error}");
            }
        }
        Some(result)
    }

    fn is_busy(&self) -> bool {
        self.pending.is_some() || self.queued.is_some()
    }

    fn shutdown(&mut self) {
        if self.shutting_down {
            return;
        }
        self.shutting_down = true;
        self.queued = None;
        let Some(pending) = self.pending.take() else {
            return;
        };
        pending
            .cancellation
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let deadline = Instant::now() + ANALYSIS_SHUTDOWN_TIMEOUT;
        let mut pending = Some(pending);
        while let Some(job) = pending.take() {
            if job.handle.is_finished() {
                let _ = job.handle.join();
                break;
            }
            if Instant::now() >= deadline {
                break;
            }
            pending = Some(job);
            thread::sleep(ANALYSIS_POLL_INTERVAL);
        }
    }
}

#[derive(Debug)]
struct PendingConfigurationRequest {
    id: RequestId,
    revision: u64,
    scope: ConfigurationScope,
}

struct ConfigurationCoordinator {
    base_options: WorkspaceOptions,
    runtime_options: RuntimeOptionsOverride,
    applied_options: WorkspaceOptions,
    pull_supported: bool,
    diagnostic_pull_supported: bool,
    initialized: bool,
    scope_uri: Option<Url>,
    accepted_scope: Option<ConfigurationScope>,
    next_request_id: u64,
    revision: u64,
    apply_revision: u64,
    refresh_pending: bool,
    pending: Option<PendingConfigurationRequest>,
    retired: VecDeque<RequestId>,
    preparation: ConfigurationPreparationJobs,
}

impl ConfigurationCoordinator {
    fn new(
        scope_uri: Option<Url>,
        options: WorkspaceOptions,
        pull_supported: bool,
        diagnostic_pull_supported: bool,
    ) -> Self {
        Self {
            base_options: options.clone(),
            runtime_options: RuntimeOptionsOverride::default(),
            applied_options: options.clone(),
            pull_supported,
            diagnostic_pull_supported,
            initialized: false,
            scope_uri,
            accepted_scope: None,
            next_request_id: 0,
            revision: 0,
            apply_revision: 0,
            refresh_pending: false,
            pending: None,
            retired: VecDeque::new(),
            preparation: ConfigurationPreparationJobs::new(),
        }
    }

    fn update_scope(
        &mut self,
        scope_uri: Option<Url>,
        workspace: &Workspace,
    ) -> Result<(), String> {
        let changed = self.scope_uri != scope_uri;
        self.scope_uri = scope_uri;
        if changed
            && self
                .accepted_scope
                .as_ref()
                .is_some_and(|scope| !scope.is_compatible_with(self.scope_uri.as_ref()))
        {
            self.runtime_options = RuntimeOptionsOverride::default();
            self.accepted_scope = None;
            self.schedule_effective_options(workspace)?;
        }
        Ok(())
    }

    fn on_initialized(
        &mut self,
        connection: &dyn ProtocolSender,
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        if self.initialized {
            return Ok(());
        }
        self.initialized = true;
        self.request_refresh(connection)
    }

    fn handle_notification(
        &mut self,
        connection: &dyn ProtocolSender,
        workspace: &mut Workspace,
        notification: &Notification,
    ) -> Result<DiagnosticNotificationEffect, Box<dyn Error + Send + Sync>> {
        let params: DidChangeConfigurationParams =
            serde_json::from_value(notification.params.clone()).map_err(|error| {
                format!("invalid parameters for workspace/didChangeConfiguration: {error}")
            })?;
        if !self.initialized {
            return Ok(DiagnosticNotificationEffect::default());
        }
        if self.pull_supported {
            self.request_refresh(connection)?;
            return Ok(DiagnosticNotificationEffect::default());
        }

        let settings = runtime_settings_section(&params.settings)?;
        self.apply_value(settings.as_ref(), workspace, ConfigurationScope::Global)
            .map_err(|error| error.into())
    }

    fn request_refresh(
        &mut self,
        connection: &dyn ProtocolSender,
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        if !self.pull_supported || !self.initialized {
            return Ok(());
        }
        self.revision = self
            .revision
            .checked_add(1)
            .ok_or_else(|| "configuration revision space exhausted".to_string())?;
        self.refresh_pending = true;
        self.send_pending(connection)
    }

    fn send_pending(
        &mut self,
        connection: &dyn ProtocolSender,
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        if !self.pull_supported
            || !self.initialized
            || !self.refresh_pending
            || self.pending.is_some()
        {
            return Ok(());
        }
        let request_number = self
            .next_request_id
            .checked_add(1)
            .ok_or_else(|| "configuration request ID space exhausted".to_string())?;
        self.next_request_id = request_number;
        let id = RequestId::from(format!("{CONFIGURATION_REQUEST_PREFIX}{request_number}"));
        let params = ConfigurationParams {
            items: vec![ConfigurationItem {
                scope_uri: self.scope_uri.clone(),
                section: Some(RUNTIME_CONFIGURATION_SECTION.to_string()),
            }],
        };
        connection.send_control(Message::Request(Request::new(
            id.clone(),
            "workspace/configuration".to_string(),
            serde_json::to_value(params)?,
        )))?;
        self.refresh_pending = false;
        self.pending = Some(PendingConfigurationRequest {
            id,
            revision: self.revision,
            scope: ConfigurationScope::from_uri(self.scope_uri.clone()),
        });
        Ok(())
    }

    fn handle_response(
        &mut self,
        connection: &dyn ProtocolSender,
        workspace: &mut Workspace,
        response: &Response,
    ) -> Result<Option<DiagnosticNotificationEffect>, Box<dyn Error + Send + Sync>> {
        if self.retired.iter().any(|id| id == &response.id) {
            eprintln!(
                "pascal-lsp: ignored duplicate workspace/configuration response {}",
                response.id
            );
            return Ok(Some(DiagnosticNotificationEffect::default()));
        }
        let Some(pending) = self.pending.as_ref() else {
            return Ok(None);
        };
        if pending.id != response.id {
            return Ok(None);
        }

        let pending = self.pending.take().expect("pending configuration request");
        self.retired.push_back(pending.id.clone());
        while self.retired.len() > MAX_RETIRED_CONFIGURATION_REQUESTS {
            self.retired.pop_front();
        }

        if pending.revision == self.revision {
            if let Some(error) = &response.error {
                eprintln!(
                    "pascal-lsp: workspace/configuration request failed ({}): {}",
                    error.code, error.message
                );
            } else if let Some(result) = response.result.as_ref() {
                self.apply_pull_result(result, workspace, pending.scope)?;
            } else {
                eprintln!(
                    "pascal-lsp: ignored malformed workspace/configuration response {}",
                    response.id
                );
            }
        }
        self.send_pending(connection)?;
        Ok(Some(DiagnosticNotificationEffect::default()))
    }

    fn apply_pull_result(
        &mut self,
        result: &Value,
        workspace: &mut Workspace,
        scope: ConfigurationScope,
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        match runtime_pull_value(result) {
            Ok(value) => self
                .apply_value(Some(&value), workspace, scope)
                .map(|_| ())
                .map_err(|error| error.into()),
            Err(error) => {
                eprintln!("pascal-lsp: ignored workspace/configuration response: {error}");
                Ok(())
            }
        }
    }

    fn apply_value(
        &mut self,
        value: Option<&Value>,
        workspace: &mut Workspace,
        scope: ConfigurationScope,
    ) -> Result<DiagnosticNotificationEffect, String> {
        let before = self.runtime_options.effective(&self.base_options);
        let previous_runtime_options = self.runtime_options.clone();
        let previous_scope = self.accepted_scope.clone();
        let warnings = match value {
            None | Some(Value::Null) => self.runtime_options.apply(RuntimeOptionsUpdate::reset()),
            Some(value) => match parse_runtime_options(value) {
                Ok(update) => self.runtime_options.apply(update),
                Err(error) => {
                    eprintln!("pascal-lsp: ignored runtime configuration: {error}");
                    return Ok(DiagnosticNotificationEffect::default());
                }
            },
        };
        for warning in warnings {
            eprintln!("pascal-lsp: warning: {warning}");
        }
        let after = self.runtime_options.effective(&self.base_options);
        self.accepted_scope = Some(scope);
        if before != after {
            if let Err(error) = self.schedule_effective_options(workspace) {
                self.runtime_options = previous_runtime_options;
                self.accepted_scope = previous_scope;
                return Err(error);
            }
        } else {
            self.schedule_effective_options(workspace)?;
        }
        Ok(DiagnosticNotificationEffect::default())
    }

    fn schedule_effective_options(&mut self, workspace: &Workspace) -> Result<(), String> {
        self.apply_revision = self
            .apply_revision
            .checked_add(1)
            .ok_or_else(|| "configuration application revision space exhausted".to_string())?;
        let options = self.runtime_options.effective(&self.base_options);
        if options == self.applied_options {
            self.preparation.cancel_pending();
            return Ok(());
        }
        self.preparation.request(ConfigurationPreparationRequest {
            revision: self.apply_revision,
            options,
            root_paths: workspace.configuration_root_paths(),
        })
    }

    fn poll(
        &mut self,
        workspace: &mut Workspace,
    ) -> Result<Option<DiagnosticNotificationEffect>, Box<dyn Error + Send + Sync>> {
        let Some(result) = self.preparation.poll() else {
            return Ok(None);
        };
        let desired = self.runtime_options.effective(&self.base_options);
        if result.revision != self.apply_revision || result.options != desired {
            return Ok(None);
        }
        if result.root_paths != workspace.configuration_root_paths() {
            self.schedule_effective_options(workspace)?;
            return Ok(None);
        }
        let prepared = match result.prepared {
            Ok(prepared) => prepared,
            Err(error) => {
                eprintln!("pascal-lsp: configuration preparation failed: {error}");
                return Ok(None);
            }
        };
        if workspace.apply_prepared_runtime_options(prepared) {
            self.applied_options = result.options;
            let mut effect = DiagnosticNotificationEffect::default();
            if self.diagnostic_pull_supported {
                effect.request_refresh();
            }
            for uri in workspace.open_document_uris() {
                effect.cancel_uri(uri.clone());
                effect.refresh_uri(uri);
            }
            return Ok(Some(effect));
        }
        if self.applied_options != result.options {
            self.schedule_effective_options(workspace)?;
        }
        Ok(None)
    }

    fn is_preparing(&self) -> bool {
        self.preparation.is_busy()
    }

    fn can_coalesce_configuration_notifications(&self) -> bool {
        self.pull_supported
    }

    fn deferred_request_revision(&self) -> u64 {
        self.apply_revision
    }

    fn shutdown(&mut self) {
        self.initialized = false;
        self.refresh_pending = false;
        self.pending = None;
        self.retired.clear();
        self.preparation.shutdown();
    }
}

fn runtime_settings_section(settings: &Value) -> Result<Option<Value>, String> {
    if settings.is_null() {
        return Ok(None);
    }
    let Some(object) = settings.as_object() else {
        return Err("settings must be an object or null".to_string());
    };
    if let Some(value) = object.get(RUNTIME_CONFIGURATION_SECTION) {
        return Ok(Some(value.clone()));
    }
    if let Some(value) = object.get("pascal-lsp") {
        return Ok(Some(value.clone()));
    }
    if [
        "sourcePaths",
        "exclude",
        "projectFile",
        "buildConfig",
        "platform",
        "compilerVersion",
        "compilerOptions",
        "conditionalDefines",
        "conditionalUndefines",
        "conditionalConstants",
        "maxFiles",
        "maxFileBytes",
        "maxTotalBytes",
    ]
    .iter()
    .any(|key| object.contains_key(*key))
    {
        return Ok(Some(settings.clone()));
    }
    Ok(None)
}

fn runtime_pull_value(result: &Value) -> Result<Value, String> {
    if result.is_null() {
        return Ok(Value::Null);
    }
    let Some(values) = result.as_array() else {
        return Err("result must be an array for the requested configuration item".to_string());
    };
    if values.len() != 1 {
        return Err("result must contain exactly one configuration item".to_string());
    }
    let value = &values[0];
    if let Some(object) = value.as_object() {
        if let Some(section) = object.get(RUNTIME_CONFIGURATION_SECTION) {
            return Ok(section.clone());
        }
        if let Some(section) = object.get("pascal-lsp") {
            return Ok(section.clone());
        }
    }
    Ok(value.clone())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct AnalysisComputationId(u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum AnalysisJobId {
    Client(AnalysisComputationId),
    Diagnostic(AnalysisComputationId),
}

#[derive(Debug, Clone)]
struct ClientRecipient {
    id: RequestId,
    work_done_token: Option<ProgressToken>,
    partial_result_token: Option<ProgressToken>,
}

#[derive(Debug, Clone, Default)]
struct AnalysisProgressTokens {
    work_done: Option<ProgressToken>,
    partial_result: Option<ProgressToken>,
}

#[derive(Debug, Clone)]
enum PartialResultPayload {
    WorkspaceSymbols(Arc<Vec<SymbolInformation>>),
    References(Arc<Vec<Location>>),
    WorkspaceDiagnostics(Arc<Vec<Value>>),
}

impl PartialResultPayload {
    fn len(&self) -> usize {
        match self {
            Self::WorkspaceSymbols(items) => items.len(),
            Self::References(items) => items.len(),
            Self::WorkspaceDiagnostics(items) => items.len(),
        }
    }

    fn item_json_len(&self, index: usize) -> Result<usize, String> {
        let bytes = match self {
            Self::WorkspaceSymbols(items) => serde_json::to_vec(&items[index]),
            Self::References(items) => serde_json::to_vec(&items[index]),
            Self::WorkspaceDiagnostics(items) => serde_json::to_vec(&items[index]),
        }
        .map_err(|error| format!("could not encode partial result item: {error}"))?
        .len();
        if bytes > MAX_PARTIAL_RESULT_ITEM_BYTES {
            return Err(format!(
                "partial result item exceeds the {MAX_PARTIAL_RESULT_ITEM_BYTES}-byte limit"
            ));
        }
        Ok(bytes)
    }

    fn retained_bytes(&self) -> Result<usize, String> {
        let mut bytes = size_of::<Self>();
        for index in 0..self.len() {
            bytes = bytes.saturating_add(self.item_json_len(index)?);
        }
        Ok(bytes)
    }

    fn chunk(&self, start: usize) -> Result<Option<(usize, Value)>, String> {
        if start >= self.len() {
            return Ok(None);
        }
        let mut end = start;
        let mut encoded_bytes: usize = 2; // The enclosing JSON array brackets.
        while end < self.len() && end.saturating_sub(start) < MAX_PARTIAL_RESULT_ITEMS_PER_CHUNK {
            let item_bytes = self.item_json_len(end)?;
            let comma_bytes = usize::from(end > start);
            let candidate = encoded_bytes
                .saturating_add(comma_bytes)
                .saturating_add(item_bytes);
            if end > start && candidate > MAX_PARTIAL_RESULT_BYTES_PER_CHUNK {
                break;
            }
            if end == start && candidate > MAX_PARTIAL_RESULT_BYTES_PER_CHUNK {
                return Err(format!(
                    "partial result item exceeds the {MAX_PARTIAL_RESULT_BYTES_PER_CHUNK}-byte chunk limit"
                ));
            }
            encoded_bytes = candidate;
            end = end.saturating_add(1);
        }

        let value = match self {
            Self::WorkspaceSymbols(items) => serde_json::to_value(&items[start..end]),
            Self::References(items) => serde_json::to_value(&items[start..end]),
            Self::WorkspaceDiagnostics(items) => {
                serde_json::to_value(serde_json::json!({"items": &items[start..end]}))
            }
        }
        .map_err(|error| format!("could not encode partial result chunk: {error}"))?;
        Ok(Some((end, value)))
    }

    fn empty_result(&self) -> Value {
        match self {
            Self::WorkspaceSymbols(_) | Self::References(_) => Value::Array(Vec::new()),
            Self::WorkspaceDiagnostics(_) => serde_json::json!({"items": []}),
        }
    }
}

#[derive(Debug)]
struct PartialDeliveryRecipient {
    id: RequestId,
    token: ProgressToken,
    next_item: usize,
}

#[derive(Debug)]
struct PartialDeliveryValidation {
    input: Arc<rename::RevalidationInput>,
    records: Arc<Vec<SourceRecord>>,
    test_barriers: TestBarrierConfig,
    cancellation: Arc<AtomicBool>,
    receiver: Option<Receiver<Result<(), String>>>,
    handle: Option<JoinHandle<()>>,
}

impl PartialDeliveryValidation {
    fn new(
        input: Arc<rename::RevalidationInput>,
        records: Arc<Vec<SourceRecord>>,
        test_barriers: TestBarrierConfig,
    ) -> Result<Self, String> {
        let mut validation = Self {
            input,
            records,
            test_barriers,
            cancellation: Arc::new(AtomicBool::new(false)),
            receiver: None,
            handle: None,
        };
        validation.request()?;
        Ok(validation)
    }

    fn request(&mut self) -> Result<(), String> {
        if self.receiver.is_some() || self.handle.is_some() {
            return Err("partial result freshness validation is already running".to_string());
        }
        let (sender, receiver) = bounded(1);
        let input = Arc::clone(&self.input);
        let records = Arc::clone(&self.records);
        let cancellation = Arc::clone(&self.cancellation);
        let test_barriers = self.test_barriers.clone();
        let handle = thread::Builder::new()
            .name("PascalLspPartialValidation".to_string())
            .spawn(move || {
                let result = match wait_at_uninterruptible_test_barrier(
                    TestBarrier::PartialValidation,
                    &test_barriers,
                ) {
                    Ok(()) => rename::revalidate_revalidation_input(
                        &input,
                        records.as_slice(),
                        &cancellation,
                    ),
                    Err(error) => Err(error),
                };
                let _ = sender.send(result);
            })
            .map_err(|error| format!("could not start partial result validation: {error}"))?;
        self.receiver = Some(receiver);
        self.handle = Some(handle);
        Ok(())
    }

    fn poll(&mut self) -> Option<Result<(), String>> {
        let receiver = self.receiver.as_ref()?;
        let result = match receiver.try_recv() {
            Ok(result) => Some(result),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => Some(Err(
                "partial result freshness validation worker disconnected".to_string(),
            )),
        }?;
        self.receiver = None;
        if let Some(handle) = self.handle.take() {
            if handle.join().is_err() {
                return Some(Err(
                    "partial result freshness validation worker panicked".to_string()
                ));
            }
        }
        Some(result)
    }

    fn cancel(&self) {
        self.cancellation
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }

    fn is_running(&self) -> bool {
        self.receiver.is_some() || self.handle.is_some()
    }
}

impl Drop for PartialDeliveryValidation {
    fn drop(&mut self) {
        self.cancellation
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

#[derive(Debug)]
struct PartialDelivery {
    job_id: AnalysisComputationId,
    source_generation: u64,
    configuration_generation: u64,
    payload: PartialResultPayload,
    retrigger_on_stale: bool,
    recipients: Vec<PartialDeliveryRecipient>,
    next_recipient: usize,
    retained_bytes: usize,
    validation: PartialDeliveryValidation,
}

#[derive(Debug)]
struct RetiredPartialValidation {
    validation: PartialDeliveryValidation,
    retained_bytes: usize,
}

fn partial_payload_from_result(
    value: AnalysisResultValue,
) -> Option<Result<PartialResultPayload, String>> {
    match value {
        AnalysisResultValue::WorkspaceSymbols(value) => {
            Some(value.map(|value| PartialResultPayload::WorkspaceSymbols(Arc::new(value))))
        }
        AnalysisResultValue::References(value) => {
            Some(value.map(|value| PartialResultPayload::References(Arc::new(value))))
        }
        AnalysisResultValue::WorkspaceDiagnostics(_) => None,
        _ => None,
    }
}

fn is_partial_result_value(value: &AnalysisResultValue) -> bool {
    matches!(
        value,
        AnalysisResultValue::WorkspaceSymbols(_)
            | AnalysisResultValue::References(_)
            | AnalysisResultValue::WorkspaceDiagnostics(_)
    )
}

fn source_records_retained_bytes(records: &[SourceRecord]) -> usize {
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

#[derive(Debug, Clone)]
enum ProgressTarget {
    Client {
        job_id: AnalysisJobId,
        request_id: RequestId,
    },
    Diagnostic {
        job_id: AnalysisJobId,
    },
}

#[derive(Debug)]
struct ProgressEntry {
    token: ProgressToken,
    target: ProgressTarget,
    title: String,
    create_id: Option<RequestId>,
    begun: bool,
}

/// Tracks work-done progress independently from request/response routing.
///
/// Request-associated tokens begin as soon as their bounded recipient is
/// admitted, including while the computation is queued.  Server-initiated
/// tokens are not visible to the client until the client acknowledges the
/// corresponding `window/workDoneProgress/create` request.  Keeping those two
/// lifecycles separate prevents a late create response from being mistaken
/// for a configuration response or from reviving a finished operation.
/// Terminal, unacknowledged create IDs retain only bounded response tombstones;
/// their cancellation targets and progress entries are retired immediately.
#[derive(Debug)]
struct ProgressTracker {
    server_supported: bool,
    next_create_id: u64,
    next_token: u64,
    entries: HashMap<AnalysisJobId, Vec<ProgressEntry>>,
    tokens: HashMap<ProgressToken, ProgressTarget>,
    creates: HashMap<RequestId, (AnalysisJobId, ProgressToken)>,
    retired_creates: HashSet<RequestId>,
}

impl ProgressTracker {
    fn new(server_supported: bool) -> Self {
        Self {
            server_supported,
            next_create_id: 0,
            next_token: 0,
            entries: HashMap::new(),
            tokens: HashMap::new(),
            creates: HashMap::new(),
            retired_creates: HashSet::new(),
        }
    }

    fn begin_client(
        &mut self,
        connection: Option<&dyn ProtocolSender>,
        job_id: AnalysisJobId,
        recipient: &ClientRecipient,
        title: &str,
    ) -> Result<(), String> {
        let (Some(connection), Some(token)) = (connection, recipient.work_done_token.as_ref())
        else {
            return Ok(());
        };
        if self.tokens.len() >= MAX_PROGRESS_ENTRIES || self.tokens.contains_key(token) {
            // A reused token cannot safely identify two concurrent operations.
            // The request itself remains valid; only its optional progress is
            // omitted until the earlier owner finishes.
            return Ok(());
        }
        let target = ProgressTarget::Client {
            job_id,
            request_id: recipient.id.clone(),
        };
        self.tokens.insert(token.clone(), target.clone());
        self.entries.entry(job_id).or_default().push(ProgressEntry {
            token: token.clone(),
            target,
            title: title.to_string(),
            create_id: None,
            begun: true,
        });
        if let Err(error) = send_progress_begin(connection, token, title, "Queued for analysis") {
            self.remove_token(token);
            return Err(error);
        }
        Ok(())
    }

    fn start_server(
        &mut self,
        connection: &dyn ProtocolSender,
        job_id: AnalysisJobId,
        title: &str,
        partial_tokens: &HashMap<ProgressToken, (AnalysisComputationId, RequestId)>,
    ) -> Result<(), String> {
        if !self.server_supported
            || self
                .creates
                .len()
                .saturating_add(self.retired_creates.len())
                >= MAX_PROGRESS_CREATES
            || self.tokens.len() >= MAX_PROGRESS_ENTRIES
        {
            return Ok(());
        }
        let token = loop {
            let token_number = self
                .next_token
                .checked_add(1)
                .ok_or_else(|| "progress token space exhausted".to_string())?;
            self.next_token = token_number;
            let token = ProgressToken::String(format!("{PROGRESS_TOKEN_PREFIX}{token_number}"));
            if partial_tokens.contains_key(&token) {
                continue;
            }
            if self.tokens.contains_key(&token) {
                return Ok(());
            }
            break token;
        };
        let request_number = self
            .next_create_id
            .checked_add(1)
            .ok_or_else(|| "progress create request ID space exhausted".to_string())?;
        self.next_create_id = request_number;
        let create_id =
            RequestId::from(format!("{PROGRESS_CREATE_REQUEST_PREFIX}{request_number}"));
        let target = ProgressTarget::Diagnostic { job_id };
        self.tokens.insert(token.clone(), target.clone());
        self.creates
            .insert(create_id.clone(), (job_id, token.clone()));
        self.entries.entry(job_id).or_default().push(ProgressEntry {
            token: token.clone(),
            target,
            title: title.to_string(),
            create_id: Some(create_id.clone()),
            begun: false,
        });
        let request = Request::new(
            create_id.clone(),
            "window/workDoneProgress/create".to_string(),
            serde_json::json!({"token": token}),
        );
        if let Err(error) = connection.send_control(Message::Request(request)) {
            self.creates.remove(&create_id);
            self.remove_token(&token);
            return Err(error.to_string());
        }
        Ok(())
    }

    fn report_started(
        &self,
        connection: &dyn ProtocolSender,
        job_id: AnalysisJobId,
    ) -> Result<(), String> {
        let tokens = self
            .entries
            .get(&job_id)
            .into_iter()
            .flatten()
            .filter(|entry| entry.begun)
            .map(|entry| entry.token.clone())
            .collect::<Vec<_>>();
        for token in tokens {
            send_progress_report(connection, &token, "Analysis started")?;
        }
        Ok(())
    }

    fn report_started_recipient(
        &self,
        connection: &dyn ProtocolSender,
        job_id: AnalysisJobId,
        request_id: &RequestId,
    ) -> Result<(), String> {
        let Some(entry) = self
            .entries
            .get(&job_id)
            .into_iter()
            .flatten()
            .find(|entry| {
                entry.begun
                    && matches!(
                        &entry.target,
                        ProgressTarget::Client {
                            request_id: entry_request_id,
                            ..
                        } if entry_request_id == request_id
                    )
            })
        else {
            return Ok(());
        };
        send_progress_report(connection, &entry.token, "Analysis started")
    }

    fn target(&self, token: &ProgressToken) -> Option<ProgressTarget> {
        self.tokens.get(token).cloned()
    }

    fn handle_create_response(
        &mut self,
        connection: &dyn ProtocolSender,
        response: &Response,
    ) -> Result<bool, String> {
        let Some((job_id, token)) = self.creates.remove(&response.id) else {
            if self.retired_creates.remove(&response.id) {
                return Ok(true);
            }
            return Ok(false);
        };
        let Some(entries) = self.entries.get_mut(&job_id) else {
            self.tokens.remove(&token);
            return Ok(true);
        };
        let Some(entry) = entries.iter_mut().find(|entry| entry.token == token) else {
            self.tokens.remove(&token);
            return Ok(true);
        };
        if response.error.is_some() {
            self.remove_token(&token);
            return Ok(true);
        }
        entry.begun = true;
        entry.create_id = None;
        let title = entry.title.clone();
        send_progress_begin(connection, &token, &title, "Indexing workspace")?;
        send_progress_report(connection, &token, "Indexing workspace")?;
        Ok(true)
    }

    fn finish_recipient(
        &mut self,
        connection: Option<&dyn ProtocolSender>,
        job_id: AnalysisJobId,
        request_id: &RequestId,
        message: Option<&str>,
    ) -> Result<(), String> {
        let Some(entries) = self.entries.get_mut(&job_id) else {
            return Ok(());
        };
        let Some(index) = entries.iter().position(|entry| {
            matches!(&entry.target, ProgressTarget::Client { request_id: id, .. } if id == request_id)
        }) else {
            return Ok(());
        };
        let entry = entries.remove(index);
        if let (Some(connection), true) = (connection, entry.begun) {
            send_progress_end(connection, &entry.token, message)?;
        }
        self.tokens.remove(&entry.token);
        if entries.is_empty() {
            self.entries.remove(&job_id);
        }
        Ok(())
    }

    fn finish_job(
        &mut self,
        connection: Option<&dyn ProtocolSender>,
        job_id: AnalysisJobId,
        message: Option<&str>,
    ) -> Result<(), String> {
        let Some(entries) = self.entries.remove(&job_id) else {
            return Ok(());
        };
        let mut first_error = None;
        for entry in entries {
            if entry.begun {
                if let Some(connection) = connection {
                    if let Err(error) = send_progress_end(connection, &entry.token, message) {
                        first_error.get_or_insert(error);
                    }
                }
            }
            if let Some(create_id) = entry.create_id {
                self.creates.remove(&create_id);
                self.retired_creates.insert(create_id);
            }
            self.tokens.remove(&entry.token);
        }
        first_error.map_or(Ok(()), Err)
    }

    fn finish_target(
        &mut self,
        connection: Option<&dyn ProtocolSender>,
        job_id: AnalysisJobId,
        token: &ProgressToken,
        message: Option<&str>,
    ) -> Result<(), String> {
        let Some(entries) = self.entries.get_mut(&job_id) else {
            self.tokens.remove(token);
            return Ok(());
        };
        let Some(index) = entries.iter().position(|entry| &entry.token == token) else {
            self.tokens.remove(token);
            return Ok(());
        };
        let entry = entries.remove(index);
        if let (Some(connection), true) = (connection, entry.begun) {
            send_progress_end(connection, &entry.token, message)?;
        }
        if let Some(create_id) = entry.create_id {
            self.creates.remove(&create_id);
            self.retired_creates.insert(create_id);
        }
        self.tokens.remove(token);
        if entries.is_empty() {
            self.entries.remove(&job_id);
        }
        Ok(())
    }

    fn remove_token(&mut self, token: &ProgressToken) {
        let Some(target) = self.tokens.remove(token) else {
            return;
        };
        let job_id = match target {
            ProgressTarget::Client { job_id, .. } | ProgressTarget::Diagnostic { job_id } => job_id,
        };
        if let Some(entries) = self.entries.get_mut(&job_id) {
            if let Some(index) = entries.iter().position(|entry| &entry.token == token) {
                if let Some(create_id) = entries[index].create_id.clone() {
                    self.creates.remove(&create_id);
                }
                entries.remove(index);
            }
            if entries.is_empty() {
                self.entries.remove(&job_id);
            }
        }
    }

    fn shutdown(&mut self, connection: Option<&dyn ProtocolSender>) -> Result<(), String> {
        let jobs = self.entries.keys().copied().collect::<Vec<_>>();
        for job_id in jobs {
            self.finish_job(connection, job_id, Some("Cancelled"))?;
        }
        // An unacknowledged create has no begun progress to close.  Drop its
        // registry entry so a late response is harmless after shutdown.
        self.entries.clear();
        self.tokens.clear();
        self.creates.clear();
        self.retired_creates.clear();
        Ok(())
    }
}

fn send_progress_begin(
    connection: &dyn ProtocolSender,
    token: &ProgressToken,
    title: &str,
    message: &str,
) -> Result<(), String> {
    connection
        .send_control(Message::Notification(Notification::new(
            "$/progress".to_string(),
            serde_json::json!({
                "token": token,
                "value": {
                    "kind": "begin",
                    "title": title,
                    "cancellable": true,
                    "message": message
                }
            }),
        )))
        .map_err(|error| error.to_string())
}

fn send_progress_report(
    connection: &dyn ProtocolSender,
    token: &ProgressToken,
    message: &str,
) -> Result<(), String> {
    connection
        .send_control(Message::Notification(Notification::new(
            "$/progress".to_string(),
            serde_json::json!({
                "token": token,
                "value": {
                    "kind": "report",
                    "cancellable": true,
                    "message": message
                }
            }),
        )))
        .map_err(|error| error.to_string())
}

fn send_progress_end(
    connection: &dyn ProtocolSender,
    token: &ProgressToken,
    message: Option<&str>,
) -> Result<(), String> {
    let value = match message {
        Some(message) => serde_json::json!({"kind": "end", "message": message}),
        None => serde_json::json!({"kind": "end"}),
    };
    connection
        .send_control(Message::Notification(Notification::new(
            "$/progress".to_string(),
            serde_json::json!({"token": token, "value": value}),
        )))
        .map_err(|error| error.to_string())
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
        snippet_support: bool,
        resolve_documentation: bool,
        resolve_detail: bool,
    },
    SignatureHelp {
        markdown: bool,
    },
    Navigation(NavigationObservationTarget),
    TypeDefinitions,
    Prepare,
    PrepareCallHierarchy,
    PrepareTypeHierarchy,
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
    InlayHints(ObservationRange),
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
                snippet_support,
                resolve_documentation,
                resolve_detail,
            } => (
                ObservationMethod::Completion {
                    markdown: matches!(format, MarkupKind::Markdown),
                    snippet_support: *snippet_support,
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
            AnalysisRequest::PrepareCallHierarchy { uri, position } => (
                ObservationMethod::PrepareCallHierarchy,
                Some(uri.clone()),
                Some(ObservationPosition {
                    line: position.line,
                    character: position.character,
                }),
                None,
                None,
            ),
            AnalysisRequest::PrepareTypeHierarchy { uri, position } => (
                ObservationMethod::PrepareTypeHierarchy,
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
            AnalysisRequest::InlayHints { uri, range } => (
                ObservationMethod::InlayHints(ObservationRange {
                    start: ObservationPosition {
                        line: range.start.line,
                        character: range.start.character,
                    },
                    end: ObservationPosition {
                        line: range.end.line,
                        character: range.end.character,
                    },
                }),
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
            | AnalysisRequest::DocumentDiagnostics { .. }
            | AnalysisRequest::WorkspaceDiagnostics { .. }
            | AnalysisRequest::Rename { .. }
            | AnalysisRequest::CodeActions(_)
            | AnalysisRequest::Resolve(_)
            | AnalysisRequest::ResolveCompletion(_)
            | AnalysisRequest::DocumentLinks { .. }
            | AnalysisRequest::IncomingCalls { .. }
            | AnalysisRequest::OutgoingCalls { .. }
            | AnalysisRequest::TypeHierarchySupertypes { .. }
            | AnalysisRequest::TypeHierarchySubtypes { .. } => return None,
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
    recipients: Vec<ClientRecipient>,
    key: Option<ObservationKey>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClientAnalysisState {
    Queued,
    Running,
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
    recipients: Vec<ClientRecipient>,
    key: Option<ObservationKey>,
}

struct PendingDiagnostic {
    uri: Url,
    analysis: PendingAnalysis,
}

struct DispatchFailure {
    recipients: Vec<ClientRecipient>,
    client_job: Option<AnalysisComputationId>,
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
    partial_deliveries: VecDeque<PartialDelivery>,
    retired_partial_validations: VecDeque<RetiredPartialValidation>,
    partial_tokens: HashMap<ProgressToken, (AnalysisComputationId, RequestId)>,
    queue: PriorityQueue<QueuedAnalysis>,
    request_to_job: HashMap<RequestId, AnalysisComputationId>,
    observation_jobs: HashMap<ObservationKey, AnalysisComputationId>,
    diagnostic_jobs: HashMap<Url, AnalysisComputationId>,
    completion_resolutions: CompletionResolutionStore,
    diagnostic_results: DiagnosticPullStore,
    progress: ProgressTracker,
    test_barriers: TestBarrierConfig,
    next_computation_id: u64,
    shutting_down: bool,
}

impl AnalysisJobs {
    #[cfg(test)]
    fn new() -> Self {
        Self::with_test_barriers_and_progress(TestBarrierConfig::disabled(), false)
    }

    fn with_test_barriers_and_progress(
        test_barriers: TestBarrierConfig,
        server_progress_supported: bool,
    ) -> Self {
        let (sender, receiver) = unbounded();
        Self {
            sender,
            receiver,
            pending: std::collections::HashMap::new(),
            diagnostics: HashMap::new(),
            partial_deliveries: VecDeque::new(),
            retired_partial_validations: VecDeque::new(),
            partial_tokens: HashMap::new(),
            queue: PriorityQueue::new(),
            request_to_job: HashMap::new(),
            observation_jobs: HashMap::new(),
            diagnostic_jobs: HashMap::new(),
            completion_resolutions: CompletionResolutionStore::new(),
            diagnostic_results: DiagnosticPullStore::new(),
            progress: ProgressTracker::new(server_progress_supported),
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
        if input.admission_fence_active {
            return Err(OPEN_ADMISSION_FENCE_MESSAGE.to_string());
        }
        let cancellation = Arc::new(AtomicBool::new(false));
        let source_generation = input.source_generation;
        let configuration_generation = input.configuration_generation;
        let worker_cancellation = Arc::clone(&cancellation);
        let related_owner_validation = match &request {
            AnalysisRequest::DocumentDiagnostics {
                related_document_support: true,
                ..
            } => self.diagnostic_results.related_owner_validation_snapshot(),
            _ => Vec::new(),
        };
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
                snippet_support,
                resolve_documentation,
                resolve_detail,
            } => AnalysisResultValue::Completion(CompletionAnalysis {
                uri: uri.clone(),
                position: *position,
                format: format.clone(),
                snippet_support: *snippet_support,
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
            AnalysisRequest::DocumentLinks { .. } => AnalysisResultValue::DocumentLinks(Err(
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
            AnalysisRequest::DocumentDiagnostics {
                uri,
                previous_result_id,
                ..
            } => AnalysisResultValue::DocumentDiagnostics(Err(format!(
                "analysis worker failed without changing workspace state for {uri} (previous result: {previous_result_id:?})"
            ))),
            AnalysisRequest::WorkspaceDiagnostics { .. } => {
                AnalysisResultValue::WorkspaceDiagnostics(Err(
                    "analysis worker failed without changing workspace state".to_string(),
                ))
            }
            AnalysisRequest::TypeDefinitions { .. } => AnalysisResultValue::TypeDefinitions(Err(
                "analysis worker failed without changing workspace state".to_string(),
            )),
            AnalysisRequest::Prepare { .. } => AnalysisResultValue::Prepare(Err(
                "analysis worker failed without changing workspace state".to_string(),
            )),
            AnalysisRequest::Rename { uri, new_uri, .. } => AnalysisResultValue::Rename {
                value: Box::new(Err(
                    "analysis worker failed without changing workspace state".to_string(),
                )),
                unit_file_move: new_uri.clone().map(|new_uri| (uri.clone(), new_uri)),
            },
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
            AnalysisRequest::InlayHints { .. } => AnalysisResultValue::InlayHints(Err(
                "analysis worker failed without changing workspace state".to_string(),
            )),
            AnalysisRequest::PrepareCallHierarchy { .. } => {
                AnalysisResultValue::PrepareCallHierarchy(Err(
                    "analysis worker failed without changing workspace state".to_string(),
                ))
            }
            AnalysisRequest::PrepareTypeHierarchy { .. } => {
                AnalysisResultValue::PrepareTypeHierarchy(Err(
                    "analysis worker panicked".to_string()
                ))
            }
            AnalysisRequest::TypeHierarchySupertypes { .. } => {
                AnalysisResultValue::TypeHierarchySupertypes(Err(
                    "analysis worker panicked".to_string()
                ))
            }
            AnalysisRequest::TypeHierarchySubtypes { .. } => {
                AnalysisResultValue::TypeHierarchySubtypes(Err(
                    "analysis worker panicked".to_string()
                ))
            }
            AnalysisRequest::IncomingCalls { .. } => AnalysisResultValue::IncomingCalls(Err(
                "analysis worker failed without changing workspace state".to_string(),
            )),
            AnalysisRequest::OutgoingCalls { .. } => AnalysisResultValue::OutgoingCalls(Err(
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
                            snippet_support,
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
                                    snippet_support,
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
                                    snippet_support,
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
                        AnalysisRequest::Formatting {
                            uri,
                            range,
                            on_type_cursor,
                            tab_size,
                            insert_spaces,
                        } => {
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
                                let computed = if let Some(range) = range {
                                    queries::range_formatting_from_input(
                                        input,
                                        &uri,
                                        range,
                                        on_type_cursor,
                                        tab_size,
                                        insert_spaces,
                                        &worker_cancellation,
                                    )
                                } else {
                                    let computed = queries::formatting_from_input(
                                        input,
                                        &uri,
                                        &worker_cancellation,
                                    );
                                    rename::Computed {
                                        source_generation: computed.source_generation,
                                        configuration_generation: computed.configuration_generation,
                                        value: computed
                                            .value
                                            .map(|edit| edit.into_iter().collect()),
                                        records: computed.records,
                                    }
                                };
                                AnalysisResult {
                                    id: worker_id,
                                    source_generation: computed.source_generation,
                                    configuration_generation: computed.configuration_generation,
                                    records: computed.records,
                                    value: AnalysisResultValue::Formatting(computed.value),
                                }
                            }
                        }
                        AnalysisRequest::DocumentLinks { uri } => {
                            let computed = queries::document_links_from_input(
                                input,
                                &uri,
                                &worker_cancellation,
                            );
                            AnalysisResult {
                                id: worker_id,
                                source_generation: computed.source_generation,
                                configuration_generation: computed.configuration_generation,
                                records: computed.records,
                                value: AnalysisResultValue::DocumentLinks(computed.value),
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
                                    value: Ok(result.publications),
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
                        AnalysisRequest::DocumentDiagnostics {
                            uri,
                            previous_result_id,
                            related_document_support,
                        } => {
                            let (source_generation, configuration_generation, records, value) =
                                match wait_at_test_barrier(
                                    TestBarrier::Diagnostics,
                                    &test_barriers,
                                    &worker_cancellation,
                                ) {
                                    Err(error) => (
                                        source_generation,
                                        configuration_generation,
                                        Vec::new(),
                                        Err(error),
                                    ),
                                    Ok(()) => {
                                        let computed = queries::diagnostics_from_input(
                                            input,
                                            &uri,
                                            &worker_cancellation,
                                        );
                                        let mut value =
                                            computed.value.and_then(|result| {
                                                prepare_diagnostic_dependencies_with_generations(
                                                    &result.publication_dependencies,
                                                    computed.source_generation,
                                                    computed.configuration_generation,
                                                )
                                                .map_err(diagnostic_validation_retry)
                                                .map(|dependencies| DocumentDiagnosticsAnalysis {
                                                    uri,
                                                    previous_result_id,
                                                    related_document_support,
                                                    publications: result.publications,
                                                    dependencies,
                                                    invalid_related_owners: Vec::new(),
                                                })
                                            });
                                        let records = if value.is_ok() {
                                            match compact_diagnostic_records(&computed.records) {
                                                Ok(records) => records,
                                                Err(error) => {
                                                    value = Err(diagnostic_validation_retry(error));
                                                    Vec::new()
                                                }
                                            }
                                        } else {
                                            Vec::new()
                                        };
                                        if value.is_ok() && related_document_support {
                                            match validate_related_owner_evidence(
                                                &validation_input,
                                                &related_owner_validation,
                                                &worker_cancellation,
                                            ) {
                                                Ok(invalid) => {
                                                    if let Ok(analysis) = value.as_mut() {
                                                        analysis.invalid_related_owners = invalid;
                                                    }
                                                }
                                                Err(error) => value = Err(error),
                                            }
                                        }
                                        (
                                            computed.source_generation,
                                            computed.configuration_generation,
                                            records,
                                            value,
                                        )
                                    }
                                };
                            AnalysisResult {
                                id: worker_id,
                                source_generation,
                                configuration_generation,
                                records,
                                value: match value {
                                    Ok(analysis) => {
                                        AnalysisResultValue::DocumentDiagnostics(Ok(analysis))
                                    }
                                    Err(error) => {
                                        AnalysisResultValue::DocumentDiagnostics(Err(error))
                                    }
                                },
                            }
                        }
                        AnalysisRequest::WorkspaceDiagnostics {
                            previous_result_ids,
                        } => {
                            let (source_generation, configuration_generation, records, value) =
                                match wait_at_test_barrier(
                                    TestBarrier::Diagnostics,
                                    &test_barriers,
                                    &worker_cancellation,
                                ) {
                                    Err(error) => (
                                        source_generation,
                                        configuration_generation,
                                        Vec::new(),
                                        Err(error),
                                    ),
                                    Ok(()) => {
                                        let computed = queries::workspace_diagnostics_from_input(
                                            input,
                                            &worker_cancellation,
                                        );
                                        let mut value =
                                            computed.value.and_then(|result| {
                                                prepare_diagnostic_dependencies_with_generations(
                                                    &result.publication_dependencies,
                                                    computed.source_generation,
                                                    computed.configuration_generation,
                                                )
                                                .map_err(diagnostic_validation_retry)
                                                .map(|dependencies| WorkspaceDiagnosticsAnalysis {
                                                    previous_result_ids,
                                                    publications: result.publications,
                                                    dependencies,
                                                })
                                            });
                                        let records = if value.is_ok() {
                                            match compact_diagnostic_records(&computed.records) {
                                                Ok(records) => records,
                                                Err(error) => {
                                                    value = Err(diagnostic_validation_retry(error));
                                                    Vec::new()
                                                }
                                            }
                                        } else {
                                            Vec::new()
                                        };
                                        (
                                            computed.source_generation,
                                            computed.configuration_generation,
                                            records,
                                            value,
                                        )
                                    }
                                };
                            AnalysisResult {
                                id: worker_id,
                                source_generation,
                                configuration_generation,
                                records,
                                value: match value {
                                    Ok(analysis) => {
                                        AnalysisResultValue::WorkspaceDiagnostics(Ok(analysis))
                                    }
                                    Err(error) => {
                                        AnalysisResultValue::WorkspaceDiagnostics(Err(error))
                                    }
                                },
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
                                    value: AnalysisResultValue::Prepare(Err(error)),
                                }
                            } else {
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
                        }
                        AnalysisRequest::Rename {
                            uri,
                            position,
                            new_name,
                            new_uri,
                        } => {
                            let unit_file_move = new_uri
                                .as_ref()
                                .map(|new_uri| (uri.clone(), new_uri.clone()));
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
                                    value: AnalysisResultValue::Rename {
                                        value: Box::new(Err(error)),
                                        unit_file_move,
                                    },
                                }
                            } else {
                                let (computed, unit_file_move) = if let Some(new_uri) = new_uri {
                                    let computed = rename::unit_rename_from_input(
                                        input,
                                        &uri,
                                        &new_uri,
                                        position,
                                        &new_name,
                                        &worker_cancellation,
                                    );
                                    (computed, Some((uri.clone(), new_uri)))
                                } else {
                                    rename::symbol_rename_from_input(
                                        input,
                                        &uri,
                                        position,
                                        &new_name,
                                        features.document_changes,
                                        features.rename_file,
                                        &worker_cancellation,
                                    )
                                };
                                AnalysisResult {
                                    id: worker_id,
                                    source_generation: computed.source_generation,
                                    configuration_generation: computed.configuration_generation,
                                    records: computed.records,
                                    value: AnalysisResultValue::Rename {
                                        value: Box::new(computed.value),
                                        unit_file_move,
                                    },
                                }
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
                                    context.snippet_support,
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
                            if let Err(error) = wait_at_test_barrier(
                                TestBarrier::WorkspaceSymbols,
                                &test_barriers,
                                &worker_cancellation,
                            ) {
                                AnalysisResult {
                                    id: worker_id,
                                    source_generation,
                                    configuration_generation,
                                    records: Vec::new(),
                                    value: AnalysisResultValue::WorkspaceSymbols(Err(error)),
                                }
                            } else {
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
                        }
                        AnalysisRequest::References {
                            uri,
                            position,
                            include_declaration,
                        } => {
                            if let Err(error) = wait_at_test_barrier(
                                TestBarrier::References,
                                &test_barriers,
                                &worker_cancellation,
                            ) {
                                AnalysisResult {
                                    id: worker_id,
                                    source_generation,
                                    configuration_generation,
                                    records: Vec::new(),
                                    value: AnalysisResultValue::References(Err(error)),
                                }
                            } else {
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
                        AnalysisRequest::InlayHints { uri, range } => {
                            let computed = queries::inlay_hints_from_input(
                                input,
                                &uri,
                                range,
                                InlayHintOptions::default(),
                                &worker_cancellation,
                            );
                            AnalysisResult {
                                id: worker_id,
                                source_generation: computed.source_generation,
                                configuration_generation: computed.configuration_generation,
                                records: computed.records,
                                value: AnalysisResultValue::InlayHints(computed.value),
                            }
                        }
                        AnalysisRequest::PrepareCallHierarchy { uri, position } => {
                            let computed = queries::prepare_call_hierarchy_from_input(
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
                                value: AnalysisResultValue::PrepareCallHierarchy(computed.value),
                            }
                        }
                        AnalysisRequest::IncomingCalls { item } => {
                            let computed = queries::incoming_calls_from_input(
                                input,
                                &item,
                                &worker_cancellation,
                            );
                            AnalysisResult {
                                id: worker_id,
                                source_generation: computed.source_generation,
                                configuration_generation: computed.configuration_generation,
                                records: computed.records,
                                value: AnalysisResultValue::IncomingCalls(computed.value),
                            }
                        }
                        AnalysisRequest::OutgoingCalls { item } => {
                            let computed = queries::outgoing_calls_from_input(
                                input,
                                &item,
                                &worker_cancellation,
                            );
                            AnalysisResult {
                                id: worker_id,
                                source_generation: computed.source_generation,
                                configuration_generation: computed.configuration_generation,
                                records: computed.records,
                                value: AnalysisResultValue::OutgoingCalls(computed.value),
                            }
                        }
                        AnalysisRequest::PrepareTypeHierarchy { uri, position } => {
                            let computed = queries::prepare_type_hierarchy_from_input(
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
                                value: AnalysisResultValue::PrepareTypeHierarchy(computed.value),
                            }
                        }
                        AnalysisRequest::TypeHierarchySupertypes { item } => {
                            let computed = queries::type_hierarchy_supertypes_from_input(
                                input,
                                &item,
                                &worker_cancellation,
                            );
                            AnalysisResult {
                                id: worker_id,
                                source_generation: computed.source_generation,
                                configuration_generation: computed.configuration_generation,
                                records: computed.records,
                                value: AnalysisResultValue::TypeHierarchySupertypes(computed.value),
                            }
                        }
                        AnalysisRequest::TypeHierarchySubtypes { item } => {
                            let computed = queries::type_hierarchy_subtypes_from_input(
                                input,
                                &item,
                                &worker_cancellation,
                            );
                            AnalysisResult {
                                id: worker_id,
                                source_generation: computed.source_generation,
                                configuration_generation: computed.configuration_generation,
                                records: computed.records,
                                value: AnalysisResultValue::TypeHierarchySubtypes(computed.value),
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
                #[cfg(feature = "test-support")]
                if matches!(
                    result.value,
                    AnalysisResultValue::DocumentLinks(_)
                        | AnalysisResultValue::InlayHints(_)
                        | AnalysisResultValue::IncomingCalls(_)
                        | AnalysisResultValue::OutgoingCalls(_)
                        | AnalysisResultValue::TypeHierarchySupertypes(_)
                        | AnalysisResultValue::TypeHierarchySubtypes(_)
                ) {
                    if let Err(error) = wait_at_test_barrier(
                        TestBarrier::PartialValidation,
                        &test_barriers,
                        &worker_cancellation,
                    ) {
                        invalidate_analysis_result(&mut result, error);
                    }
                }
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
            recipients: Vec::new(),
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
        self.enqueue_client(id, request, workspace, features, None, None)
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
            .map(|job| job.recipients.len())
            .sum::<usize>();
        let queued = self
            .queue
            .iter()
            .map(|job| match job {
                QueuedAnalysis::Client(job) => job.recipients.len(),
                QueuedAnalysis::Diagnostic(_) => 0,
            })
            .sum::<usize>();
        let delivering = self
            .partial_deliveries
            .iter()
            .map(|delivery| delivery.recipients.len())
            .sum::<usize>();
        running
            .saturating_add(queued)
            .saturating_add(delivering)
            .saturating_add(self.retired_partial_validations.len())
    }

    fn partial_delivery_bytes(&self) -> usize {
        let delivering = self
            .partial_deliveries
            .iter()
            .map(|delivery| delivery.retained_bytes)
            .sum::<usize>();
        let retiring = self
            .retired_partial_validations
            .iter()
            .map(|validation| validation.retained_bytes)
            .sum::<usize>();
        delivering.saturating_add(retiring)
    }

    fn reap_retired_partial_validations(&mut self) {
        let mut remaining = VecDeque::new();
        while let Some(mut retired) = self.retired_partial_validations.pop_front() {
            if retired.validation.poll().is_none() {
                remaining.push_back(retired);
            }
        }
        self.retired_partial_validations = remaining;
    }

    fn retire_partial_validation(
        &mut self,
        validation: PartialDeliveryValidation,
        retained_bytes: usize,
    ) {
        if !validation.is_running() {
            return;
        }
        validation.cancel();
        debug_assert!(
            self.retired_partial_validations.len() < MAX_PARTIAL_VALIDATION_RETIREMENTS,
            "retiring partial validation bound must be checked before removal"
        );
        self.retired_partial_validations
            .push_back(RetiredPartialValidation {
                validation,
                retained_bytes,
            });
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

    #[cfg(test)]
    fn enqueue_client(
        &mut self,
        id: RequestId,
        request: AnalysisRequest,
        workspace: &Workspace,
        features: ClientFeatures,
        work_done_token: Option<ProgressToken>,
        connection: Option<&dyn ProtocolSender>,
    ) -> Result<(), String> {
        self.enqueue_client_with_partial(
            id,
            request,
            workspace,
            features,
            AnalysisProgressTokens {
                work_done: work_done_token,
                partial_result: None,
            },
            connection,
        )
    }

    fn enqueue_client_with_partial(
        &mut self,
        id: RequestId,
        request: AnalysisRequest,
        workspace: &Workspace,
        features: ClientFeatures,
        tokens: AnalysisProgressTokens,
        connection: Option<&dyn ProtocolSender>,
    ) -> Result<(), String> {
        self.reap_retired_partial_validations();
        if self.shutting_down {
            return Err("analysis server is shutting down".to_string());
        }
        if workspace.analysis_admission_fenced() {
            return Err(OPEN_ADMISSION_FENCE_MESSAGE.to_string());
        }
        if self.request_to_job.contains_key(&id) {
            return Err("analysis request ID is already in use".to_string());
        }
        if tokens.partial_result.is_some()
            && !matches!(
                &request,
                AnalysisRequest::WorkspaceSymbols { .. }
                    | AnalysisRequest::References { .. }
                    | AnalysisRequest::WorkspaceDiagnostics { .. }
            )
        {
            return Err("partialResultToken is unsupported for this request".to_string());
        }
        if tokens.partial_result.as_ref() == tokens.work_done.as_ref()
            && tokens.partial_result.is_some()
        {
            return Err("workDoneToken and partialResultToken must be distinct".to_string());
        }
        if tokens
            .partial_result
            .as_ref()
            .is_some_and(|token| self.partial_tokens.contains_key(token))
            || tokens
                .partial_result
                .as_ref()
                .is_some_and(|token| self.progress.target(token).is_some())
            || tokens
                .work_done
                .as_ref()
                .is_some_and(|token| self.partial_tokens.contains_key(token))
        {
            return Err("progress token is already in use by another operation".to_string());
        }

        let title = progress_title(&request).to_string();
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
                if self.client_recipient_count() >= MAX_CLIENT_ANALYSIS_RECIPIENTS
                    || self.partial_tokens.len() >= MAX_CLIENT_ANALYSIS_RECIPIENTS
                {
                    return Err(ANALYSIS_QUEUE_FULL_MESSAGE.to_string());
                }
                let recipient = ClientRecipient {
                    id: id.clone(),
                    work_done_token: tokens.work_done.clone(),
                    partial_result_token: tokens.partial_result.clone(),
                };
                if let Some(state) = self.attach_client(&primary_id, recipient.clone()) {
                    self.request_to_job.insert(id, primary_id);
                    self.register_partial_token(&recipient, primary_id);
                    self.progress.begin_client(
                        connection,
                        AnalysisJobId::Client(primary_id),
                        &recipient,
                        &title,
                    )?;
                    if state == ClientAnalysisState::Running {
                        if let Some(connection) = connection {
                            self.progress.report_started_recipient(
                                connection,
                                AnalysisJobId::Client(primary_id),
                                &recipient.id,
                            )?;
                        }
                    }
                    return Ok(());
                }
                self.observation_jobs.remove(key);
            }
        }

        if self.client_recipient_count() >= MAX_CLIENT_ANALYSIS_RECIPIENTS
            || self.partial_tokens.len() >= MAX_CLIENT_ANALYSIS_RECIPIENTS
            || self.queue.len() >= MAX_CLIENT_ANALYSIS_QUEUE
        {
            return Err(ANALYSIS_QUEUE_FULL_MESSAGE.to_string());
        }

        let primary_id = self.next_computation_id()?;
        self.request_to_job.insert(id.clone(), primary_id);
        let recipient = ClientRecipient {
            id,
            work_done_token: tokens.work_done,
            partial_result_token: tokens.partial_result,
        };
        if let Some(key) = key.clone() {
            self.observation_jobs.insert(key, primary_id);
        }
        self.queue.push(
            AnalysisPriority::for_request(&request),
            QueuedAnalysis::Client(QueuedClientAnalysis {
                id: primary_id,
                request,
                features,
                recipients: vec![recipient.clone()],
                key,
            }),
        );
        self.register_partial_token(&recipient, primary_id);
        self.progress.begin_client(
            connection,
            AnalysisJobId::Client(primary_id),
            &recipient,
            &title,
        )?;
        let failures = self.pump(workspace, connection);
        self.handle_dispatch_failures(failures, connection)
    }

    fn completion_resolution_request(
        &self,
        item: &CompletionItem,
    ) -> Result<CompletionResolutionRequest, String> {
        self.completion_resolutions.request(item)
    }

    fn attach_client(
        &mut self,
        primary_id: &AnalysisComputationId,
        recipient: ClientRecipient,
    ) -> Option<ClientAnalysisState> {
        if let Some(job) = self.pending.get_mut(primary_id) {
            if !job.cancellation.load(std::sync::atomic::Ordering::Relaxed) {
                job.recipients.push(recipient);
                return Some(ClientAnalysisState::Running);
            }
        }
        if let Some(QueuedAnalysis::Client(job)) = self.queue.find_mut(
            |queued| matches!(queued, QueuedAnalysis::Client(job) if &job.id == primary_id),
        ) {
            job.recipients.push(recipient);
            return Some(ClientAnalysisState::Queued);
        }
        None
    }

    fn register_partial_token(
        &mut self,
        recipient: &ClientRecipient,
        primary_id: AnalysisComputationId,
    ) {
        if let Some(token) = recipient.partial_result_token.as_ref() {
            let previous = self
                .partial_tokens
                .insert(token.clone(), (primary_id, recipient.id.clone()));
            debug_assert!(
                previous.is_none(),
                "partial token was checked before admission"
            );
        }
    }

    fn release_partial_token(&mut self, recipient: &ClientRecipient) {
        let Some(token) = recipient.partial_result_token.as_ref() else {
            return;
        };
        if self
            .partial_tokens
            .get(token)
            .is_some_and(|(_, request_id)| request_id == &recipient.id)
        {
            self.partial_tokens.remove(token);
        }
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
        connection: Option<&dyn ProtocolSender>,
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
        connection: Option<&dyn ProtocolSender>,
    ) -> Result<(), String> {
        if let Some(QueuedAnalysis::Client(job)) = self.queue.remove_first(
            |queued| matches!(queued, QueuedAnalysis::Client(job) if &job.id == primary_id),
        ) {
            self.remove_observation(job.key.as_ref(), primary_id);
            let request_ids = job
                .recipients
                .iter()
                .map(|recipient| recipient.id.clone())
                .collect::<Vec<_>>();
            for id in &request_ids {
                self.remove_client_mapping(id, primary_id);
            }
            Self::send_client_error(
                connection,
                request_ids,
                ErrorCode::RequestCanceled,
                ANALYSIS_SUPERSEDED_MESSAGE,
            )?;
            for recipient in job.recipients {
                self.release_partial_token(&recipient);
                self.progress.finish_recipient(
                    connection,
                    AnalysisJobId::Client(*primary_id),
                    &recipient.id,
                    Some("Superseded"),
                )?;
            }
            return Ok(());
        }

        let Some(job) = self.pending.get_mut(primary_id) else {
            return Ok(());
        };
        job.cancellation
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let key = job.key.clone();
        let recipients = std::mem::take(&mut job.recipients);
        self.remove_observation(key.as_ref(), primary_id);
        let request_ids = recipients
            .iter()
            .map(|recipient| recipient.id.clone())
            .collect::<Vec<_>>();
        for id in &request_ids {
            self.remove_client_mapping(id, primary_id);
        }
        Self::send_client_error(
            connection,
            request_ids,
            ErrorCode::RequestCanceled,
            ANALYSIS_SUPERSEDED_MESSAGE,
        )?;
        for recipient in recipients {
            self.release_partial_token(&recipient);
            self.progress.finish_recipient(
                connection,
                AnalysisJobId::Client(*primary_id),
                &recipient.id,
                Some("Superseded"),
            )?;
        }
        Ok(())
    }

    fn cancel(&mut self, connection: &dyn ProtocolSender, id: &RequestId) -> Result<(), String> {
        self.reap_retired_partial_validations();
        let Some(primary_id) = self.request_to_job.get(id).cloned() else {
            return Ok(());
        };

        let mut queued_empty = false;
        let mut found_queued = false;
        let mut cancelled_recipient = None;
        if let Some(QueuedAnalysis::Client(job)) = self.queue.find_mut(
            |queued| matches!(queued, QueuedAnalysis::Client(job) if job.id == primary_id),
        ) {
            if let Some(position) = job
                .recipients
                .iter()
                .position(|recipient| &recipient.id == id)
            {
                cancelled_recipient = job.recipients.get(position).cloned();
                job.recipients.remove(position);
                queued_empty = job.recipients.is_empty();
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
            self.progress.finish_recipient(
                Some(connection),
                AnalysisJobId::Client(primary_id),
                id,
                Some("Cancelled"),
            )?;
            if let Some(recipient) = cancelled_recipient.as_ref() {
                self.release_partial_token(recipient);
            }
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
        let mut cancelled_recipient = None;
        if let Some(job) = self.pending.get_mut(&primary_id) {
            if let Some(position) = job
                .recipients
                .iter()
                .position(|recipient| &recipient.id == id)
            {
                cancelled_recipient = job.recipients.get(position).cloned();
                job.recipients.remove(position);
                cancel_worker = job.recipients.is_empty();
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
            self.progress.finish_recipient(
                Some(connection),
                AnalysisJobId::Client(primary_id),
                id,
                Some("Cancelled"),
            )?;
            if let Some(recipient) = cancelled_recipient.as_ref() {
                self.release_partial_token(recipient);
            }
            return Ok(());
        }

        if let Some(index) = self.partial_deliveries.iter().position(|delivery| {
            delivery
                .recipients
                .iter()
                .any(|recipient| &recipient.id == id)
        }) {
            let mut delivery = self
                .partial_deliveries
                .remove(index)
                .expect("partial delivery selected for cancellation");
            let recipient_index = delivery
                .recipients
                .iter()
                .position(|recipient| &recipient.id == id)
                .expect("partial recipient selected for cancellation");
            let recipient = delivery.recipients.remove(recipient_index);
            self.remove_client_mapping(id, &primary_id);
            let delivery_job_id = delivery.job_id;
            if delivery.recipients.is_empty() {
                let PartialDelivery {
                    validation,
                    retained_bytes,
                    ..
                } = delivery;
                self.retire_partial_validation(validation, retained_bytes);
                send_error(
                    connection,
                    id.clone(),
                    ErrorCode::RequestCanceled,
                    rename::CANCELLATION_MESSAGE,
                )
                .map_err(|error| error.to_string())?;
                self.finish_partial_recipient(connection, delivery_job_id, &recipient)
                    .map_err(|error| error.to_string())?;
            } else {
                send_error(
                    connection,
                    id.clone(),
                    ErrorCode::RequestCanceled,
                    rename::CANCELLATION_MESSAGE,
                )
                .map_err(|error| error.to_string())?;
                self.finish_partial_recipient(connection, delivery_job_id, &recipient)
                    .map_err(|error| error.to_string())?;
                delivery.next_recipient %= delivery.recipients.len();
                self.partial_deliveries.insert(index, delivery);
            }
            return Ok(());
        }
        self.request_to_job.remove(id);
        Ok(())
    }

    fn cancel_progress(
        &mut self,
        connection: &dyn ProtocolSender,
        token: &ProgressToken,
    ) -> Result<(), String> {
        let Some(target) = self.progress.target(token) else {
            return Ok(());
        };
        match target {
            ProgressTarget::Client { request_id, .. } => self.cancel(connection, &request_id),
            ProgressTarget::Diagnostic { job_id } => {
                if let AnalysisJobId::Diagnostic(id) = job_id {
                    if let Some(QueuedAnalysis::Diagnostic(job)) = self.queue.remove_first(
                        |queued| matches!(queued, QueuedAnalysis::Diagnostic(job) if job.id == id),
                    ) {
                        if self
                            .diagnostic_jobs
                            .get(&job.uri)
                            .is_some_and(|job_id| *job_id == id)
                        {
                            self.diagnostic_jobs.remove(&job.uri);
                        }
                    } else if let Some(job) = self.diagnostics.get(&id) {
                        job.analysis
                            .cancellation
                            .store(true, std::sync::atomic::Ordering::Relaxed);
                    }
                    self.progress.finish_target(
                        Some(connection),
                        job_id,
                        token,
                        Some("Cancelled"),
                    )?;
                }
                Ok(())
            }
        }
    }

    fn handle_progress_response(
        &mut self,
        connection: &dyn ProtocolSender,
        response: &Response,
    ) -> Result<bool, String> {
        self.progress.handle_create_response(connection, response)
    }

    fn cancel_diagnostics_for_with_connection(
        &mut self,
        connection: Option<&dyn ProtocolSender>,
        uris: &[Url],
    ) -> Result<(), String> {
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
        let running = self
            .diagnostics
            .iter()
            .filter(|(_, diagnostic)| uris.contains(&diagnostic.uri))
            .map(|(id, diagnostic)| {
                diagnostic
                    .analysis
                    .cancellation
                    .store(true, std::sync::atomic::Ordering::Relaxed);
                *id
            })
            .collect::<Vec<_>>();
        for id in running {
            self.progress.finish_job(
                connection,
                AnalysisJobId::Diagnostic(id),
                Some("Invalidated"),
            )?;
        }
        Ok(())
    }

    fn cancel_all_diagnostics_with_connection(
        &mut self,
        connection: Option<&dyn ProtocolSender>,
    ) -> Result<(), String> {
        // Active/queued analysis is already capped by MAX_ANALYSIS_JOBS; this
        // vector is bounded by that admission ceiling, unlike the workspace's
        // open-document set.
        let uris = self.diagnostic_jobs.keys().cloned().collect::<Vec<_>>();
        self.cancel_diagnostics_for_with_connection(connection, &uris)
    }

    fn refresh_diagnostics_with_connection(
        &mut self,
        connection: &dyn ProtocolSender,
        workspace: &mut Workspace,
        uris: &[Url],
    ) -> Result<(), String> {
        self.cancel_diagnostics_for_with_connection(Some(connection), uris)?;
        for uri in uris {
            workspace.reschedule_diagnostics(uri.clone());
        }
        Ok(())
    }

    fn pump(
        &mut self,
        workspace: &Workspace,
        connection: Option<&dyn ProtocolSender>,
    ) -> Vec<DispatchFailure> {
        if self.shutting_down {
            return Vec::new();
        }
        let mut failures = Vec::new();
        if workspace.analysis_admission_fenced() {
            while let Some(queued) = self.queue.pop() {
                match queued {
                    QueuedAnalysis::Client(job) => {
                        self.remove_observation(job.key.as_ref(), &job.id);
                        for recipient in &job.recipients {
                            self.remove_client_mapping(&recipient.id, &job.id);
                        }
                        failures.push(DispatchFailure {
                            recipients: job.recipients,
                            client_job: Some(job.id),
                            diagnostic: None,
                            message: OPEN_ADMISSION_FENCE_MESSAGE.to_string(),
                        });
                    }
                    QueuedAnalysis::Diagnostic(job) => {
                        self.diagnostic_jobs.remove(&job.uri);
                    }
                }
            }
            return failures;
        }
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
                            analysis.recipients = job.recipients;
                            analysis.key = job.key;
                            self.pending.insert(primary_id, analysis);
                            if let Some(connection) = connection {
                                if let Err(error) = self
                                    .progress
                                    .report_started(connection, AnalysisJobId::Client(primary_id))
                                {
                                    failures.push(DispatchFailure {
                                        recipients: Vec::new(),
                                        client_job: Some(primary_id),
                                        diagnostic: None,
                                        message: error,
                                    });
                                }
                            }
                        }
                        Err(message) => {
                            self.remove_observation(job.key.as_ref(), &primary_id);
                            for recipient in &job.recipients {
                                self.remove_client_mapping(&recipient.id, &primary_id);
                            }
                            failures.push(DispatchFailure {
                                recipients: job.recipients,
                                client_job: Some(primary_id),
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
                            if let Some(connection) = connection {
                                if let Err(error) = self.progress.start_server(
                                    connection,
                                    AnalysisJobId::Diagnostic(id),
                                    "Indexing workspace",
                                    &self.partial_tokens,
                                ) {
                                    failures.push(DispatchFailure {
                                        recipients: Vec::new(),
                                        client_job: None,
                                        diagnostic: None,
                                        message: error,
                                    });
                                }
                            }
                            self.diagnostics
                                .insert(id, PendingDiagnostic { uri, analysis });
                        }
                        Err(message) => {
                            self.diagnostic_jobs.remove(&uri);
                            failures.push(DispatchFailure {
                                recipients: Vec::new(),
                                client_job: None,
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
        connection: Option<&dyn ProtocolSender>,
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
                for recipient in &failure.recipients {
                    if let Err(error) = send_error(
                        connection,
                        recipient.id.clone(),
                        ErrorCode::RequestFailed,
                        &failure.message,
                    ) {
                        first_error.get_or_insert_with(|| error.to_string());
                    }
                    if let Some(client_job) = failure.client_job {
                        self.remove_client_mapping(&recipient.id, &client_job);
                        self.release_partial_token(recipient);
                        if let Err(error) = self.progress.finish_recipient(
                            Some(connection),
                            AnalysisJobId::Client(client_job),
                            &recipient.id,
                            Some("Failed"),
                        ) {
                            first_error.get_or_insert(error);
                        }
                    }
                }
            } else if first_error.is_none() && !failure.recipients.is_empty() {
                first_error = Some(failure.message);
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    fn finish_partial_recipient(
        &mut self,
        connection: &dyn ProtocolSender,
        job_id: AnalysisComputationId,
        recipient: &PartialDeliveryRecipient,
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        self.remove_client_mapping(&recipient.id, &job_id);
        self.release_partial_token(&ClientRecipient {
            id: recipient.id.clone(),
            work_done_token: None,
            partial_result_token: Some(recipient.token.clone()),
        });
        self.progress
            .finish_recipient(
                Some(connection),
                AnalysisJobId::Client(job_id),
                &recipient.id,
                None,
            )
            .map_err(|error| error.into())
    }

    fn fail_client_recipients(
        &mut self,
        connection: &dyn ProtocolSender,
        job_id: AnalysisComputationId,
        recipients: Vec<ClientRecipient>,
        code: ErrorCode,
        message: &str,
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        for recipient in recipients {
            self.remove_client_mapping(&recipient.id, &job_id);
            send_error(connection, recipient.id.clone(), code, message)?;
            self.release_partial_token(&recipient);
            self.progress
                .finish_recipient(
                    Some(connection),
                    AnalysisJobId::Client(job_id),
                    &recipient.id,
                    Some("Failed"),
                )
                .map_err(|error| -> Box<dyn Error + Send + Sync> { error.into() })?;
        }
        Ok(())
    }

    fn fail_diagnostic_recipients(
        &mut self,
        connection: &dyn ProtocolSender,
        job_id: AnalysisComputationId,
        recipients: Vec<ClientRecipient>,
        message: &str,
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        for recipient in recipients {
            self.remove_client_mapping(&recipient.id, &job_id);
            send_diagnostic_server_cancelled(connection, recipient.id.clone(), message)?;
            self.release_partial_token(&recipient);
            self.progress
                .finish_recipient(
                    Some(connection),
                    AnalysisJobId::Client(job_id),
                    &recipient.id,
                    Some("Failed"),
                )
                .map_err(|error| -> Box<dyn Error + Send + Sync> { error.into() })?;
        }
        Ok(())
    }

    fn fail_partial_delivery(
        &mut self,
        connection: &dyn ProtocolSender,
        delivery: PartialDelivery,
        code: ErrorCode,
        message: &str,
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        let PartialDelivery {
            job_id,
            recipients,
            retained_bytes,
            validation,
            ..
        } = delivery;
        self.retire_partial_validation(validation, retained_bytes);
        let recipients = recipients
            .into_iter()
            .map(|recipient| ClientRecipient {
                id: recipient.id,
                work_done_token: None,
                partial_result_token: Some(recipient.token),
            })
            .collect();
        self.fail_client_recipients(connection, job_id, recipients, code, message)
    }

    fn fail_partial_delivery_with_retrigger(
        &mut self,
        connection: &dyn ProtocolSender,
        delivery: PartialDelivery,
        message: &str,
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        let PartialDelivery {
            job_id,
            recipients,
            retained_bytes,
            validation,
            ..
        } = delivery;
        self.retire_partial_validation(validation, retained_bytes);
        let recipients = recipients
            .into_iter()
            .map(|recipient| ClientRecipient {
                id: recipient.id,
                work_done_token: None,
                partial_result_token: Some(recipient.token),
            })
            .collect();
        self.fail_diagnostic_recipients(connection, job_id, recipients, message)
    }

    fn send_bulk_result(
        connection: &dyn ProtocolSender,
        id: RequestId,
        payload: &PartialResultPayload,
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        match payload {
            PartialResultPayload::WorkspaceSymbols(items) => send_ok(connection, id, &**items),
            PartialResultPayload::References(items) => send_ok(connection, id, &**items),
            PartialResultPayload::WorkspaceDiagnostics(items) => {
                send_ok(connection, id, serde_json::json!({"items": &**items}))
            }
        }
    }

    fn start_partial_delivery(
        &mut self,
        connection: &dyn ProtocolSender,
        workspace: &Workspace,
        result: AnalysisResult,
        recipients: Vec<ClientRecipient>,
        primary_id: AnalysisComputationId,
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        if analysis_result_is_stale(workspace, &result) {
            if matches!(&result.value, AnalysisResultValue::WorkspaceDiagnostics(_)) {
                return self.fail_diagnostic_recipients(
                    connection,
                    primary_id,
                    recipients,
                    "analysis result became stale; retry the request",
                );
            }
            return self.fail_client_recipients(
                connection,
                primary_id,
                recipients,
                ErrorCode::RequestFailed,
                "analysis result became stale; retry the request",
            );
        }

        let AnalysisResult {
            source_generation,
            configuration_generation,
            records,
            value,
            ..
        } = result;
        let payload = match value {
            AnalysisResultValue::WorkspaceDiagnostics(value) => {
                let analysis = match value {
                    Ok(analysis) => analysis,
                    Err(error) => {
                        for recipient in recipients {
                            self.remove_client_mapping(&recipient.id, &primary_id);
                            send_diagnostic_analysis_error(
                                connection,
                                recipient.id.clone(),
                                error.clone(),
                            )?;
                            self.release_partial_token(&recipient);
                            self.progress
                                .finish_recipient(
                                    Some(connection),
                                    AnalysisJobId::Client(primary_id),
                                    &recipient.id,
                                    Some("Failed"),
                                )
                                .map_err(|error| -> Box<dyn Error + Send + Sync> {
                                    error.into()
                                })?;
                        }
                        return Ok(());
                    }
                };
                match workspace_diagnostic_items(workspace, &mut self.diagnostic_results, analysis)
                {
                    Ok(items) => PartialResultPayload::WorkspaceDiagnostics(Arc::new(items)),
                    Err(error) => {
                        return self.fail_client_recipients(
                            connection,
                            primary_id,
                            recipients,
                            ErrorCode::RequestFailed,
                            &error,
                        );
                    }
                }
            }
            value => {
                let Some(payload) = partial_payload_from_result(value) else {
                    return self.fail_client_recipients(
                        connection,
                        primary_id,
                        recipients,
                        ErrorCode::RequestFailed,
                        "partial result was attached to an unsupported analysis",
                    );
                };
                match payload {
                    Ok(payload) => payload,
                    Err(error) => {
                        for recipient in recipients {
                            self.remove_client_mapping(&recipient.id, &primary_id);
                            send_analysis_error(connection, recipient.id.clone(), error.clone())?;
                            self.release_partial_token(&recipient);
                            self.progress
                                .finish_recipient(
                                    Some(connection),
                                    AnalysisJobId::Client(primary_id),
                                    &recipient.id,
                                    Some("Failed"),
                                )
                                .map_err(|error| -> Box<dyn Error + Send + Sync> {
                                    error.into()
                                })?;
                        }
                        return Ok(());
                    }
                }
            }
        };

        let mut partial_recipients = Vec::new();
        let mut ordinary_recipients = Vec::new();
        for recipient in recipients {
            if let Some(token) = recipient.partial_result_token.clone() {
                if payload.len() == 0 {
                    send_ok(connection, recipient.id.clone(), payload.empty_result())?;
                    self.remove_client_mapping(&recipient.id, &primary_id);
                    self.release_partial_token(&recipient);
                    self.progress
                        .finish_recipient(
                            Some(connection),
                            AnalysisJobId::Client(primary_id),
                            &recipient.id,
                            None,
                        )
                        .map_err(|error| -> Box<dyn Error + Send + Sync> { error.into() })?;
                } else {
                    partial_recipients.push(PartialDeliveryRecipient {
                        id: recipient.id,
                        token,
                        next_item: 0,
                    });
                }
            } else {
                ordinary_recipients.push(recipient);
            }
        }

        for recipient in ordinary_recipients {
            Self::send_bulk_result(connection, recipient.id.clone(), &payload)?;
            self.remove_client_mapping(&recipient.id, &primary_id);
            self.release_partial_token(&recipient);
            self.progress
                .finish_recipient(
                    Some(connection),
                    AnalysisJobId::Client(primary_id),
                    &recipient.id,
                    None,
                )
                .map_err(|error| -> Box<dyn Error + Send + Sync> { error.into() })?;
        }

        if !partial_recipients.is_empty() {
            let revalidation_input = Arc::new(workspace.revalidation_input());
            let partial_failure = payload.retained_bytes().map(|payload_bytes| {
                payload_bytes
                    .saturating_add(source_records_retained_bytes(&records))
                    .saturating_add(revalidation_input.retained_bytes())
            });
            let retained_bytes = match partial_failure {
                Ok(retained_bytes) => retained_bytes,
                Err(error) => {
                    let recipients = partial_recipients
                        .into_iter()
                        .map(|recipient| ClientRecipient {
                            id: recipient.id,
                            work_done_token: None,
                            partial_result_token: Some(recipient.token),
                        })
                        .collect();
                    return self.fail_client_recipients(
                        connection,
                        primary_id,
                        recipients,
                        ErrorCode::RequestFailed,
                        &error,
                    );
                }
            };
            if self
                .partial_deliveries
                .len()
                .saturating_add(self.retired_partial_validations.len())
                >= MAX_PARTIAL_VALIDATION_RETIREMENTS
            {
                let recipients = partial_recipients
                    .into_iter()
                    .map(|recipient| ClientRecipient {
                        id: recipient.id,
                        work_done_token: None,
                        partial_result_token: Some(recipient.token),
                    })
                    .collect();
                return self.fail_client_recipients(
                    connection,
                    primary_id,
                    recipients,
                    ErrorCode::RequestFailed,
                    "partial result validation capacity is full; retry the request",
                );
            }
            if self.partial_delivery_bytes().saturating_add(retained_bytes)
                > MAX_PARTIAL_DELIVERY_BYTES
            {
                let recipients = partial_recipients
                    .into_iter()
                    .map(|recipient| ClientRecipient {
                        id: recipient.id,
                        work_done_token: None,
                        partial_result_token: Some(recipient.token),
                    })
                    .collect();
                return self.fail_client_recipients(
                    connection,
                    primary_id,
                    recipients,
                    ErrorCode::RequestFailed,
                    "partial result delivery capacity is full; retry the request",
                );
            }
            let records = Arc::new(records);
            let validation = match PartialDeliveryValidation::new(
                revalidation_input,
                records,
                self.test_barriers.clone(),
            ) {
                Ok(validation) => validation,
                Err(error) => {
                    let recipients = partial_recipients
                        .into_iter()
                        .map(|recipient| ClientRecipient {
                            id: recipient.id,
                            work_done_token: None,
                            partial_result_token: Some(recipient.token),
                        })
                        .collect();
                    return self.fail_client_recipients(
                        connection,
                        primary_id,
                        recipients,
                        ErrorCode::RequestFailed,
                        &error,
                    );
                }
            };
            let retrigger_on_stale =
                matches!(&payload, PartialResultPayload::WorkspaceDiagnostics(_));
            self.partial_deliveries.push_back(PartialDelivery {
                job_id: primary_id,
                source_generation,
                configuration_generation,
                payload,
                retrigger_on_stale,
                recipients: partial_recipients,
                next_recipient: 0,
                retained_bytes,
                validation,
            });
        }
        Ok(())
    }

    fn pump_partial_deliveries(
        &mut self,
        connection: &dyn ProtocolSender,
        workspace: &Workspace,
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        self.reap_retired_partial_validations();
        for _ in 0..MAX_PARTIAL_RESULT_CHUNKS_PER_TURN {
            let Some(mut delivery) = self.partial_deliveries.pop_front() else {
                return Ok(());
            };
            if workspace.analysis_admission_fenced()
                || delivery.source_generation != workspace.source_generation()
                || delivery.configuration_generation != workspace.configuration_generation()
            {
                if delivery.retrigger_on_stale {
                    self.fail_partial_delivery_with_retrigger(
                        connection,
                        delivery,
                        "analysis result became stale during partial delivery; retry the request",
                    )?;
                } else {
                    self.fail_partial_delivery(
                        connection,
                        delivery,
                        ErrorCode::RequestFailed,
                        "analysis result became stale during partial delivery; retry the request",
                    )?;
                }
                continue;
            }
            if delivery.recipients.is_empty() {
                continue;
            }
            match delivery.validation.poll() {
                None => {
                    self.partial_deliveries.push_front(delivery);
                    return Ok(());
                }
                Some(Err(error)) => {
                    let message =
                        format!("analysis result became stale during partial delivery: {error}");
                    if delivery.retrigger_on_stale {
                        self.fail_partial_delivery_with_retrigger(connection, delivery, &message)?;
                    } else {
                        self.fail_partial_delivery(
                            connection,
                            delivery,
                            ErrorCode::RequestFailed,
                            &message,
                        )?;
                    }
                    continue;
                }
                Some(Ok(())) => {}
            }
            let recipient_index = delivery.next_recipient % delivery.recipients.len();
            let next_item = delivery.recipients[recipient_index].next_item;
            if next_item >= delivery.payload.len() {
                let recipient = delivery.recipients.remove(recipient_index);
                send_ok(
                    connection,
                    recipient.id.clone(),
                    delivery.payload.empty_result(),
                )?;
                self.finish_partial_recipient(connection, delivery.job_id, &recipient)?;
                if !delivery.recipients.is_empty() {
                    delivery.next_recipient %= delivery.recipients.len();
                    delivery.validation.request()?;
                    self.partial_deliveries.push_back(delivery);
                }
                continue;
            }
            let (end, value) = match delivery.payload.chunk(next_item) {
                Ok(Some(chunk)) => chunk,
                Ok(None) => unreachable!("partial result cursor checked above"),
                Err(error) => {
                    self.fail_partial_delivery(
                        connection,
                        delivery,
                        ErrorCode::RequestFailed,
                        &error,
                    )?;
                    continue;
                }
            };
            let token = delivery.recipients[recipient_index].token.clone();
            if !send_partial_result_chunk(connection, &token, value)? {
                delivery.validation.request()?;
                self.partial_deliveries.push_front(delivery);
                return Ok(());
            }
            delivery.recipients[recipient_index].next_item = end;
            if !delivery.recipients.is_empty() {
                delivery.next_recipient = (recipient_index + 1) % delivery.recipients.len();
                delivery.validation.request()?;
                self.partial_deliveries.push_back(delivery);
            }
        }
        Ok(())
    }

    #[cfg(test)]
    fn poll(
        &mut self,
        connection: &dyn ProtocolSender,
        workspace: &mut Workspace,
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        let mut diagnostic_budget = DiagnosticPublicationTurnBudget::default();
        self.poll_with_diagnostic_budget(connection, workspace, &mut diagnostic_budget, true)
    }

    fn poll_with_diagnostic_budget(
        &mut self,
        connection: &dyn ProtocolSender,
        workspace: &mut Workspace,
        diagnostic_budget: &mut DiagnosticPublicationTurnBudget,
        allow_normal_publication: bool,
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        let staling = DiagnosticPublicationBatchSender::new(connection, true);
        let result = self.poll_diagnostic_turn_inner(&staling, workspace);
        let did_scan = staling.has_staling_work();
        let scan = staling.flush();
        if did_scan {
            diagnostic_budget.publication_queue_scans =
                diagnostic_budget.publication_queue_scans.saturating_add(1);
            diagnostic_budget.publication_queue_messages_scanned = diagnostic_budget
                .publication_queue_messages_scanned
                .saturating_add(scan.scanned_messages);
            diagnostic_budget.publication_queue_bytes_scanned = diagnostic_budget
                .publication_queue_bytes_scanned
                .saturating_add(scan.scanned_bytes);
        }
        if staling.is_stale_all() {
            workspace.invalidate_all_for_file_notification_overflow_bounded();
            staling.flush_deferred_progress()?;
            return Err("diagnostic staling exceeded its retained-target bound".into());
        }
        let result = match result {
            Ok(()) => {
                if allow_normal_publication {
                    pump_pending_diagnostic_publications_with_budget(
                        connection,
                        workspace,
                        diagnostic_budget,
                    )
                } else {
                    Ok(())
                }
            }
            Err(error) => Err(error),
        };
        let progress_result = staling.flush_deferred_progress();
        result?;
        progress_result?;
        Ok(())
    }

    fn poll_diagnostic_turn_inner(
        &mut self,
        connection: &dyn ProtocolSender,
        workspace: &mut Workspace,
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        self.reap_retired_partial_validations();
        if workspace.analysis_admission_fenced() {
            // A rejected editor overlay invalidates authority workspace-wide.
            // Stop active workers before inspecting their completions so even
            // results that were already computed cannot be published.
            for job in self.pending.values() {
                job.cancellation.store(true, Ordering::Relaxed);
            }
            for job in self.diagnostics.values() {
                job.analysis.cancellation.store(true, Ordering::Relaxed);
            }
        }
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
                        self.progress
                            .finish_job(
                                Some(connection),
                                AnalysisJobId::Diagnostic(id),
                                Some("Cancelled"),
                            )
                            .map_err(|error| -> Box<dyn Error + Send + Sync> { error.into() })?;
                        if !workspace.analysis_admission_fenced() {
                            workspace.reschedule_diagnostics(job.uri);
                        }
                    } else {
                        deliver_analysis_result_with_store(
                            connection,
                            workspace,
                            &mut self.completion_resolutions,
                            &mut self.diagnostic_results,
                            result,
                            None,
                        )?;
                        self.progress
                            .finish_job(Some(connection), AnalysisJobId::Diagnostic(id), None)
                            .map_err(|error| -> Box<dyn Error + Send + Sync> { error.into() })?;
                    }
                }
                AnalysisJobId::Client(primary_id) => {
                    let Some(job) = self.pending.remove(&primary_id) else {
                        continue;
                    };
                    let cancelled = job.cancellation.load(std::sync::atomic::Ordering::Relaxed);
                    let recipients = job.recipients;
                    let key = job.key;
                    let _ = job.handle.join();
                    self.remove_observation(key.as_ref(), &primary_id);
                    if cancelled {
                        for recipient in &recipients {
                            self.remove_client_mapping(&recipient.id, &primary_id);
                        }
                        Self::send_client_error(
                            Some(connection),
                            recipients.iter().map(|recipient| recipient.id.clone()),
                            ErrorCode::RequestCanceled,
                            rename::CANCELLATION_MESSAGE,
                        )
                        .map_err(|error| -> Box<dyn Error + Send + Sync> { error.into() })?;
                        for recipient in &recipients {
                            self.release_partial_token(recipient);
                            self.progress
                                .finish_recipient(
                                    Some(connection),
                                    AnalysisJobId::Client(primary_id),
                                    &recipient.id,
                                    Some("Cancelled"),
                                )
                                .map_err(|error| -> Box<dyn Error + Send + Sync> {
                                    error.into()
                                })?;
                        }
                    } else if !recipients.is_empty() {
                        if recipients
                            .iter()
                            .any(|recipient| recipient.partial_result_token.is_some())
                            && is_partial_result_value(&result.value)
                        {
                            self.start_partial_delivery(
                                connection, workspace, result, recipients, primary_id,
                            )?;
                        } else {
                            for recipient in &recipients {
                                deliver_analysis_result_with_store(
                                    connection,
                                    workspace,
                                    &mut self.completion_resolutions,
                                    &mut self.diagnostic_results,
                                    result.clone(),
                                    Some(recipient.id.clone()),
                                )?;
                                self.remove_client_mapping(&recipient.id, &primary_id);
                                self.release_partial_token(recipient);
                                self.progress
                                    .finish_recipient(
                                        Some(connection),
                                        AnalysisJobId::Client(primary_id),
                                        &recipient.id,
                                        None,
                                    )
                                    .map_err(|error| -> Box<dyn Error + Send + Sync> {
                                        error.into()
                                    })?;
                            }
                        }
                    }
                }
            }
        }
        let failures = self.pump(workspace, Some(connection));
        self.handle_dispatch_failures(failures, Some(connection))
            .map_err(|error| -> Box<dyn Error + Send + Sync> { error.into() })?;
        Ok(())
    }

    fn is_empty(&self) -> bool {
        self.pending.is_empty()
            && self.diagnostics.is_empty()
            && self.queue.is_empty()
            && self.partial_deliveries.is_empty()
            && self.retired_partial_validations.is_empty()
    }

    fn shutdown(&mut self) {
        let _ = self.shutdown_inner(None);
    }

    fn shutdown_with_connection(
        &mut self,
        connection: &dyn ProtocolSender,
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        self.shutdown_inner(Some(connection))
            .map_err(|error| -> Box<dyn Error + Send + Sync> { error.into() })
    }

    fn shutdown_inner(&mut self, connection: Option<&dyn ProtocolSender>) -> Result<(), String> {
        if self.shutting_down {
            return Ok(());
        }
        self.shutting_down = true;
        let mut queued = std::mem::take(&mut self.queue);
        while let Some(job) = queued.pop() {
            if let QueuedAnalysis::Client(job) = job {
                let request_ids = job
                    .recipients
                    .iter()
                    .map(|recipient| recipient.id.clone())
                    .collect::<Vec<_>>();
                Self::send_client_error(
                    connection,
                    request_ids,
                    ErrorCode::RequestCanceled,
                    rename::CANCELLATION_MESSAGE,
                )?;
                for recipient in job.recipients {
                    self.release_partial_token(&recipient);
                    self.progress.finish_recipient(
                        connection,
                        AnalysisJobId::Client(job.id),
                        &recipient.id,
                        Some("Cancelled"),
                    )?;
                }
            }
        }
        let partial_deliveries = std::mem::take(&mut self.partial_deliveries);
        for delivery in partial_deliveries {
            let PartialDelivery {
                job_id,
                recipients,
                retained_bytes,
                validation,
                ..
            } = delivery;
            self.retire_partial_validation(validation, retained_bytes);
            for recipient in recipients {
                let request_id = recipient.id.clone();
                if let Some(connection) = connection {
                    send_error(
                        connection,
                        request_id.clone(),
                        ErrorCode::RequestCanceled,
                        rename::CANCELLATION_MESSAGE,
                    )
                    .map_err(|error| error.to_string())?;
                }
                self.remove_client_mapping(&request_id, &job_id);
                self.release_partial_token(&ClientRecipient {
                    id: request_id.clone(),
                    work_done_token: None,
                    partial_result_token: Some(recipient.token),
                });
                self.progress.finish_recipient(
                    connection,
                    AnalysisJobId::Client(job_id),
                    &request_id,
                    Some("Cancelled"),
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
            let request_ids = job
                .recipients
                .iter()
                .map(|recipient| recipient.id.clone())
                .collect::<Vec<_>>();
            Self::send_client_error(
                connection,
                request_ids,
                ErrorCode::RequestCanceled,
                rename::CANCELLATION_MESSAGE,
            )?;
            for recipient in &job.recipients {
                self.release_partial_token(recipient);
            }
        }

        self.progress.shutdown(connection)?;
        self.partial_tokens.clear();

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
        while !self.retired_partial_validations.is_empty() {
            self.reap_retired_partial_validations();
            if self.retired_partial_validations.is_empty() || Instant::now() >= deadline {
                break;
            }
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
        rename_file: false,
        will_rename_files: false,
        hierarchical_document_symbols: false,
        hover_format: DocumentationFormat::PlainText,
        completion_format: DocumentationFormat::PlainText,
        completion_snippet_support: false,
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
                | AnalysisResultValue::DocumentLinks(_)
                | AnalysisResultValue::Diagnostics(_)
                | AnalysisResultValue::DocumentDiagnostics(_)
                | AnalysisResultValue::WorkspaceDiagnostics(_)
                | AnalysisResultValue::TypeDefinitions(_)
                | AnalysisResultValue::CodeActions(_)
                | AnalysisResultValue::Resolve(_)
                | AnalysisResultValue::DocumentSymbols { .. }
                | AnalysisResultValue::DocumentHighlights(_)
                | AnalysisResultValue::SelectionRanges(_)
                | AnalysisResultValue::SemanticTokens(_)
                | AnalysisResultValue::FoldingRanges(_)
                | AnalysisResultValue::InlayHints(_)
                | AnalysisResultValue::PrepareCallHierarchy(_)
                | AnalysisResultValue::IncomingCalls(_)
                | AnalysisResultValue::OutgoingCalls(_)
        )
}

fn analysis_result_is_stale(workspace: &Workspace, result: &AnalysisResult) -> bool {
    if workspace.analysis_admission_fenced() {
        return true;
    }
    if is_dependency_scoped_result(&result.value, &result.records) {
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
    }
}

fn send_partial_result_chunk(
    connection: &dyn ProtocolSender,
    token: &ProgressToken,
    value: Value,
) -> Result<bool, Box<dyn Error + Send + Sync>> {
    let message = Message::Notification(Notification::new(
        "$/progress".to_string(),
        serde_json::json!({"token": token, "value": value}),
    ));
    connection
        .send_data(message)
        .map_err(|error| -> Box<dyn Error + Send + Sync> { error.into() })
}

#[cfg(test)]
fn deliver_analysis_result(
    connection: &dyn ProtocolSender,
    workspace: &mut Workspace,
    result: AnalysisResult,
    client_id: Option<RequestId>,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let mut completion_resolutions = CompletionResolutionStore::new();
    let mut diagnostic_results = DiagnosticPullStore::new();
    deliver_analysis_result_with_store(
        connection,
        workspace,
        &mut completion_resolutions,
        &mut diagnostic_results,
        result,
        client_id,
    )
}

fn deliver_analysis_result_with_store(
    connection: &dyn ProtocolSender,
    workspace: &mut Workspace,
    completion_resolutions: &mut CompletionResolutionStore,
    diagnostic_results: &mut DiagnosticPullStore,
    result: AnalysisResult,
    client_id: Option<RequestId>,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let stale = analysis_result_is_stale(workspace, &result);
    if stale {
        match &result.value {
            AnalysisResultValue::Diagnostics(diagnostics) => {
                workspace.reschedule_diagnostics(diagnostics.uri.clone());
                return Ok(());
            }
            AnalysisResultValue::DocumentDiagnostics(_)
            | AnalysisResultValue::WorkspaceDiagnostics(_) => {
                return send_diagnostic_server_cancelled(
                    connection,
                    client_id
                        .clone()
                        .expect("stale diagnostic pull has a client result"),
                    "analysis result became stale; retry the request",
                );
            }
            _ => {}
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
        workspace.record_diagnostic_dependencies(diagnostics.uri.clone(), result.records.clone());
    }
    if let AnalysisResultValue::DocumentDiagnostics(Ok(diagnostics)) = &result.value {
        workspace.record_diagnostic_dependencies(diagnostics.uri.clone(), result.records.clone());
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
            Ok(edits) => send_ok(connection, client_id.clone().expect("client result"), edits),
            Err(error) if error == rename::CANCELLATION_MESSAGE => {
                send_analysis_error(connection, client_id.clone().expect("client result"), error)
            }
            Err(error)
                if error.starts_with("range start")
                    || error.starts_with("range end")
                    || error.starts_with("formatting tabSize") =>
            {
                send_error(
                    connection,
                    client_id.clone().expect("client result"),
                    ErrorCode::InvalidParams,
                    format!("invalid range formatting parameters: {error}"),
                )
            }
            Err(error) => send_error(
                connection,
                client_id.clone().expect("client result"),
                ErrorCode::RequestFailed,
                format!("formatting failed: {error}"),
            ),
        },
        AnalysisResultValue::DocumentLinks(value) => match value {
            Ok(value) => send_ok(connection, client_id.clone().expect("client result"), value),
            Err(error) => {
                send_analysis_error(connection, client_id.clone().expect("client result"), error)
            }
        },
        AnalysisResultValue::Diagnostics(diagnostics) => match diagnostics.value {
            Ok(publications) => {
                send_diagnostic_publications(connection, workspace, &diagnostics.uri, publications)
            }
            Err(error) if error == rename::CANCELLATION_MESSAGE => {
                workspace.reschedule_diagnostics(diagnostics.uri);
                Ok(())
            }
            Err(error) => {
                let uri = diagnostics.uri;
                let version = diagnostics.version;
                let updates = workspace
                    .stage_diagnostic_publications(
                        &uri,
                        std::iter::once(queries::DiagnosticPublication {
                            uri: uri.clone(),
                            version,
                            diagnostics: vec![crate::workspace::server_diagnostic(
                                &error,
                                lsp_types::DiagnosticSeverity::ERROR,
                            )],
                        }),
                    )
                    .map_err(|error| -> Box<dyn Error + Send + Sync> { error.into() })?;
                if updates.incomplete {
                    workspace.mark_pending_diagnostic_publication_incomplete();
                }
                Ok(())
            }
        },
        AnalysisResultValue::DocumentDiagnostics(value) => match value {
            Ok(analysis) => send_document_diagnostics(
                connection,
                workspace,
                diagnostic_results,
                analysis,
                client_id.expect("client result"),
            ),
            Err(error) => {
                send_diagnostic_analysis_error(connection, client_id.expect("client result"), error)
            }
        },
        AnalysisResultValue::WorkspaceDiagnostics(value) => match value {
            Ok(analysis) => send_workspace_diagnostics(
                connection,
                workspace,
                diagnostic_results,
                analysis,
                client_id.expect("client result"),
            ),
            Err(error) => {
                send_diagnostic_analysis_error(connection, client_id.expect("client result"), error)
            }
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
        AnalysisResultValue::Rename {
            value,
            unit_file_move,
        } => match *value {
            Ok(value) => {
                if let Some((old_uri, new_uri)) = unit_file_move {
                    if let Err(error) = workspace.stage_unit_file_rename(&old_uri, &new_uri, &value)
                    {
                        return send_analysis_error(
                            connection,
                            client_id.clone().expect("client result"),
                            error,
                        );
                    }
                    let response =
                        send_ok(connection, client_id.clone().expect("client result"), value);
                    if response.is_err() {
                        workspace.cancel_staged_unit_file_rename(&old_uri, &new_uri);
                    }
                    response
                } else {
                    send_ok(connection, client_id.clone().expect("client result"), value)
                }
            }
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
        AnalysisResultValue::InlayHints(value) => match value {
            Ok(value) => send_ok(connection, client_id.clone().expect("client result"), value),
            Err(error) => {
                send_analysis_error(connection, client_id.clone().expect("client result"), error)
            }
        },
        AnalysisResultValue::PrepareCallHierarchy(value) => match value {
            Ok(value) => send_ok(connection, client_id.clone().expect("client result"), value),
            Err(error) => {
                send_analysis_error(connection, client_id.clone().expect("client result"), error)
            }
        },
        AnalysisResultValue::PrepareTypeHierarchy(value)
        | AnalysisResultValue::TypeHierarchySupertypes(value)
        | AnalysisResultValue::TypeHierarchySubtypes(value) => match value {
            Ok(value) => send_ok(connection, client_id.clone().expect("client result"), value),
            Err(error) => {
                send_analysis_error(connection, client_id.clone().expect("client result"), error)
            }
        },
        AnalysisResultValue::IncomingCalls(value) => match value {
            Ok(value) => send_ok(connection, client_id.clone().expect("client result"), value),
            Err(error) => {
                send_analysis_error(connection, client_id.clone().expect("client result"), error)
            }
        },
        AnalysisResultValue::OutgoingCalls(value) => match value {
            Ok(value) => send_ok(connection, client_id.clone().expect("client result"), value),
            Err(error) => {
                send_analysis_error(connection, client_id.clone().expect("client result"), error)
            }
        },
    }
}

#[allow(clippy::too_many_arguments)]
fn send_document_diagnostics(
    connection: &dyn ProtocolSender,
    workspace: &Workspace,
    diagnostic_results: &mut DiagnosticPullStore,
    analysis: DocumentDiagnosticsAnalysis,
    client_id: RequestId,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let DocumentDiagnosticsAnalysis {
        uri,
        previous_result_id,
        related_document_support,
        publications,
        dependencies,
        invalid_related_owners,
    } = analysis;
    let publication = publications
        .iter()
        .find(|publication| publication.uri == uri)
        .cloned()
        .unwrap_or_else(|| queries::DiagnosticPublication {
            uri: uri.clone(),
            version: None,
            diagnostics: Vec::new(),
        });
    let previous = previous_result_id
        .as_deref()
        .and_then(|previous| diagnostic_results.get(previous))
        .cloned();
    let root_dependency = dependencies.get(&publication.uri);
    let mut next_id = diagnostic_results.next_id;
    let entry = match diagnostic_results.preview_insert(
        publication.uri,
        publication.version,
        publication.diagnostics,
        root_dependency,
        &mut next_id,
    ) {
        Ok(entry) => entry,
        Err(error) => return send_error(connection, client_id, ErrorCode::RequestFailed, error),
    };
    let unchanged = entry.cacheable
        && previous.is_some_and(|previous| {
            previous.result_id == entry.result_id
                && previous.uri == entry.uri
                && previous.diagnostics_identity == entry.diagnostics_identity
                && previous.records_identity == entry.records_identity
            // The worker has already re-read and fingerprinted the
            // effective dependency set for this response.  A protocol
            // version or watcher generation alone must not defeat an
            // otherwise identical report (notably a no-op overlay edit).
        });
    let mut value = if unchanged {
        serde_json::json!({
            "kind": "unchanged",
            "resultId": entry.result_id.clone(),
        })
    } else {
        serde_json::json!({
            "kind": "full",
            "resultId": entry.result_id.clone(),
            "items": entry.diagnostics.clone(),
        })
    };
    let related_publications = related_document_support
        .then(|| {
            publications
                .into_iter()
                .filter(|publication| publication.uri != entry.uri)
                .map(|publication| {
                    let dependency = dependencies.get(&publication.uri).cloned();
                    (publication.uri, publication.diagnostics, dependency)
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let reconciliation = diagnostic_results.reconcile_related_owners(invalid_related_owners);
    let owner_transaction = match diagnostic_results.prepare_related_owner(
        &uri,
        related_publications,
        reconciliation,
    ) {
        Ok(transaction) => transaction,
        Err(error) => return send_error(connection, client_id, ErrorCode::RequestFailed, error),
    };
    let mut entries = vec![entry.clone()];
    if related_document_support {
        let mut related = serde_json::Map::new();
        for report in &owner_transaction.reports {
            let dependency = report.dependency.as_ref();
            let report_uri = report.uri.clone();
            let related_entry = match diagnostic_results.preview_insert(
                report_uri.clone(),
                workspace.document_version(&report_uri),
                report.diagnostics.clone(),
                dependency,
                &mut next_id,
            ) {
                Ok(entry) => entry,
                Err(error) => {
                    return send_error(connection, client_id, ErrorCode::RequestFailed, error);
                }
            };
            related.insert(
                related_entry.uri.to_string(),
                serde_json::json!({
                    "kind": "full",
                    "resultId": related_entry.result_id.clone(),
                    "items": related_entry.diagnostics.clone(),
                }),
            );
            entries.push(related_entry);
        }
        if !related.is_empty() {
            if let Some(object) = value.as_object_mut() {
                object.insert("relatedDocuments".to_string(), Value::Object(related));
            }
        }
    }
    let fallback_id = client_id.clone();
    match connection.send_result(Message::Response(Response::new_ok(client_id, value))) {
        Ok(()) => {
            // Both owner state and result-cache entries become visible only
            // after the complete response has been admitted to bounded output.
            // A failed replacement therefore retains the old owner/URI set so
            // a later successful request can still deliver its clear.
            diagnostic_results.commit_related_owner(owner_transaction);
            diagnostic_results.commit_entries(entries, next_id);
            Ok(())
        }
        Err(OutputError::MessageTooLarge) => send_error(
            connection,
            fallback_id,
            ErrorCode::RequestFailed,
            "analysis result exceeds the bounded LSP output size; retry with a narrower request",
        ),
        Err(OutputError::ResultBackpressure) => send_error(
            connection,
            fallback_id,
            ErrorCode::RequestFailed,
            "temporary LSP output capacity is full; retry the request after the client drains",
        ),
        Err(error) => Err(error.into()),
    }
}

#[allow(clippy::too_many_arguments)]
fn send_workspace_diagnostics(
    connection: &dyn ProtocolSender,
    workspace: &Workspace,
    diagnostic_results: &mut DiagnosticPullStore,
    analysis: WorkspaceDiagnosticsAnalysis,
    client_id: RequestId,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let items = match workspace_diagnostic_items(workspace, diagnostic_results, analysis) {
        Ok(items) => items,
        Err(error) => {
            return send_error(connection, client_id, ErrorCode::RequestFailed, error);
        }
    };
    send_ok(connection, client_id, serde_json::json!({"items": items}))
}

fn workspace_diagnostic_items(
    workspace: &Workspace,
    diagnostic_results: &mut DiagnosticPullStore,
    analysis: WorkspaceDiagnosticsAnalysis,
) -> Result<Vec<Value>, String> {
    let previous = analysis
        .previous_result_ids
        .into_iter()
        .collect::<HashMap<Url, String>>();
    let mut seen = HashSet::new();
    let mut items = Vec::with_capacity(analysis.publications.len());
    for publication in analysis.publications {
        let old = previous
            .get(&publication.uri)
            .and_then(|result_id| diagnostic_results.get(result_id))
            .cloned();
        let dependency = analysis.dependencies.get(&publication.uri);
        let entry = diagnostic_results.insert(
            publication.uri.clone(),
            publication.version,
            publication.diagnostics,
            dependency,
        )?;
        seen.insert(publication.uri.clone());
        let unchanged = entry.cacheable
            && old.is_some_and(|old| {
                old.result_id == previous[&entry.uri]
                    && old.result_id == entry.result_id
                    && old.diagnostics_identity == entry.diagnostics_identity
                    && old.records_identity == entry.records_identity
            });
        if unchanged {
            items.push(serde_json::json!({
                "kind": "unchanged",
                "uri": entry.uri,
                "version": entry.version.map(i64::from),
                "resultId": entry.result_id,
            }));
        } else {
            items.push(serde_json::json!({
                "kind": "full",
                "uri": entry.uri,
                "version": entry.version.map(i64::from),
                "resultId": entry.result_id,
                "items": entry.diagnostics,
            }));
        }

        if items.len() > MAX_DIAGNOSTIC_REPORT_ITEMS {
            return Err(format!(
                "workspace diagnostic report exceeds the {MAX_DIAGNOSTIC_REPORT_ITEMS}-item limit"
            ));
        }
    }

    // A workspace request must explicitly clear documents that were reported
    // by the client previously but are no longer an authorized/current source.
    // The URI is supplied by the client, so this branch never reads it.
    let mut missing = previous
        .keys()
        .filter(|uri| !seen.contains(*uri))
        .cloned()
        .collect::<Vec<_>>();
    missing.sort_by(|left, right| left.as_str().cmp(right.as_str()));
    let empty_dependency: Option<DiagnosticDependency> = None;
    for uri in missing {
        let entry = diagnostic_results.insert(
            uri.clone(),
            workspace.document_version(&uri),
            Vec::new(),
            empty_dependency.as_ref(),
        )?;
        items.push(serde_json::json!({
            "kind": "full",
            "uri": entry.uri,
            "version": entry.version.map(i64::from),
            "resultId": entry.result_id,
            "items": entry.diagnostics,
        }));
        if items.len() > MAX_DIAGNOSTIC_REPORT_ITEMS {
            return Err(format!(
                "workspace diagnostic report exceeds the {MAX_DIAGNOSTIC_REPORT_ITEMS}-item limit"
            ));
        }
    }
    items.sort_by(|left, right| {
        left["uri"]
            .as_str()
            .unwrap_or_default()
            .cmp(right["uri"].as_str().unwrap_or_default())
    });

    let mut encoded_bytes = 2usize; // The enclosing JSON array brackets.
    for item in &items {
        let item_bytes = serde_json::to_vec(item)
            .map_err(|error| format!("could not encode workspace diagnostic report: {error}"))?
            .len();
        if item_bytes > MAX_DIAGNOSTIC_REPORT_ITEM_BYTES {
            return Err(format!(
                "workspace diagnostic report item exceeds the {MAX_DIAGNOSTIC_REPORT_ITEM_BYTES}-byte limit"
            ));
        }
        encoded_bytes = encoded_bytes
            .saturating_add(item_bytes)
            .saturating_add(usize::from(encoded_bytes > 2));
        if encoded_bytes > MAX_DIAGNOSTIC_REPORT_BYTES {
            return Err(format!(
                "workspace diagnostic report exceeds the {MAX_DIAGNOSTIC_REPORT_BYTES}-byte limit"
            ));
        }
    }
    Ok(items)
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
        AnalysisResultValue::DocumentLinks(value) => *value = Err(error),
        AnalysisResultValue::Diagnostics(diagnostics) => {
            diagnostics.value = Err(error);
            diagnostics.discard = true;
        }
        AnalysisResultValue::DocumentDiagnostics(value) => *value = Err(error),
        AnalysisResultValue::WorkspaceDiagnostics(value) => *value = Err(error),
        AnalysisResultValue::TypeDefinitions(value) => *value = Err(error),
        AnalysisResultValue::Prepare(value) => *value = Err(error),
        AnalysisResultValue::Rename { value, .. } => **value = Err(error),
        AnalysisResultValue::CodeActions(value) => *value = Err(error),
        AnalysisResultValue::Resolve(value) => **value = Err(error),
        AnalysisResultValue::DocumentSymbols { value, .. } => *value = Err(error),
        AnalysisResultValue::WorkspaceSymbols(value) => *value = Err(error),
        AnalysisResultValue::References(value) => *value = Err(error),
        AnalysisResultValue::DocumentHighlights(value) => *value = Err(error),
        AnalysisResultValue::SelectionRanges(value) => *value = Err(error),
        AnalysisResultValue::SemanticTokens(value) => *value = Err(error),
        AnalysisResultValue::FoldingRanges(value) => *value = Err(error),
        AnalysisResultValue::InlayHints(value) => *value = Err(error),
        AnalysisResultValue::PrepareCallHierarchy(value) => *value = Err(error),
        AnalysisResultValue::IncomingCalls(value) => *value = Err(error),
        AnalysisResultValue::OutgoingCalls(value) => *value = Err(error),
        AnalysisResultValue::PrepareTypeHierarchy(value)
        | AnalysisResultValue::TypeHierarchySupertypes(value)
        | AnalysisResultValue::TypeHierarchySubtypes(value) => *value = Err(error),
    }
}

fn send_analysis_error(
    connection: &dyn ProtocolSender,
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

fn send_diagnostic_analysis_error(
    connection: &dyn ProtocolSender,
    id: RequestId,
    error: String,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    if error.starts_with(DIAGNOSTIC_VALIDATION_RETRY_PREFIX) {
        return send_diagnostic_server_cancelled(connection, id, error);
    }
    if error != rename::CANCELLATION_MESSAGE {
        return send_error(connection, id, ErrorCode::RequestFailed, error);
    }
    send_diagnostic_server_cancelled(connection, id, error)
}

fn send_diagnostic_server_cancelled(
    connection: &dyn ProtocolSender,
    id: RequestId,
    message: impl Into<String>,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let mut response = Response::new_err(id, ErrorCode::ServerCancelled as i32, message.into());
    if let Some(response_error) = response.error.as_mut() {
        response_error.data = Some(serde_json::json!({"retriggerRequest": true}));
    }
    connection.send_control(Message::Response(response))?;
    Ok(())
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
    let (connection, priority_receiver, io_threads) = bounded_stdio(test_barriers.clone());
    let connection = ProtocolConnection::new(connection, priority_receiver, &test_barriers);
    let outcome = run_connection(&connection, test_barriers);
    let shutdown_deadline = Instant::now() + OUTPUT_SHUTDOWN_TIMEOUT;
    let drain_result = connection.drain_until(shutdown_deadline);
    drop(connection);
    let join_result = io_threads.join_until(shutdown_deadline);
    match outcome {
        Ok(success) => {
            drain_result?;
            join_result?;
            Ok(success)
        }
        Err(error) => {
            let _ = join_result;
            let _ = drain_result;
            Err(error)
        }
    }
}

struct StdioThreads {
    reader: JoinHandle<io::Result<()>>,
    writer: JoinHandle<io::Result<()>>,
}

impl StdioThreads {
    fn join_until(self, deadline: Instant) -> io::Result<()> {
        let mut reader = Some(self.reader);
        let mut writer = Some(self.writer);
        while reader.is_some() || writer.is_some() {
            if reader
                .as_ref()
                .is_some_and(std::thread::JoinHandle::is_finished)
            {
                let handle = reader.take().expect("reader handle");
                match handle.join() {
                    Ok(result) => result?,
                    Err(error) => std::panic::panic_any(error),
                }
            }
            if writer
                .as_ref()
                .is_some_and(std::thread::JoinHandle::is_finished)
            {
                let handle = writer.take().expect("writer handle");
                match handle.join() {
                    Ok(result) => result?,
                    Err(error) => std::panic::panic_any(error),
                }
            }
            if reader.is_none() && writer.is_none() {
                return Ok(());
            }
            if Instant::now() >= deadline {
                // A peer that does not drain stdout must not keep the process
                // alive indefinitely. Dropping an unfinished JoinHandle
                // detaches the blocked transport thread; process teardown
                // then closes the descriptors and bounds shutdown time.
                return Ok(());
            }
            thread::sleep(ANALYSIS_POLL_INTERVAL);
        }
        Ok(())
    }
}

fn bounded_stdio(
    test_barriers: TestBarrierConfig,
) -> (Connection, Receiver<Message>, StdioThreads) {
    #[cfg(feature = "test-support")]
    let reader_test_barriers = test_barriers.clone();
    let (writer_sender, writer_receiver) = bounded::<Message>(MAX_OUTBOUND_MESSAGES);
    let writer = thread::Builder::new()
        .name("PascalLspWriter".to_string())
        .spawn(move || {
            let stdout = io::stdout();
            let mut stdout = stdout.lock();
            #[cfg(feature = "test-support")]
            let outbound_writer_barrier = test_barriers.outbound_writer;
            #[cfg(feature = "test-support")]
            let mut outbound_writer_barrier_used = false;
            #[cfg(not(feature = "test-support"))]
            let _ = test_barriers;
            for message in writer_receiver {
                #[cfg(feature = "test-support")]
                if !outbound_writer_barrier_used
                    && outbound_writer_barrier
                        .as_ref()
                        .is_some_and(|barrier| barrier.armed.exists())
                {
                    let barrier = outbound_writer_barrier
                        .as_ref()
                        .expect("armed outbound writer barrier exists");
                    std::fs::write(&barrier.entered, b"entered")?;
                    while !barrier.release.exists() {
                        thread::sleep(ANALYSIS_POLL_INTERVAL);
                    }
                    outbound_writer_barrier_used = true;
                }
                message.write(&mut stdout)?;
            }
            Ok(())
        })
        .expect("spawn LSP writer");

    let (reader_sender, reader_receiver) = bounded::<Message>(0);
    let (priority_sender, priority_receiver) = bounded::<Message>(8);
    let reader = thread::Builder::new()
        .name("PascalLspReader".to_string())
        .spawn(move || {
            let stdin = io::stdin();
            let mut stdin = BoundedReader::new(stdin.lock());
            while let Some(message) = Message::read(&mut stdin)? {
                let is_exit = is_exit_notification(&message);
                let dispatch = reader_sender.try_send(message);
                match dispatch {
                    Ok(()) => {}
                    Err(TrySendError::Disconnected(_)) => return Ok(()),
                    Err(TrySendError::Full(message)) if is_priority_control_message(&message) => {
                        if priority_sender.send(message).is_err() {
                            return Ok(());
                        }
                    }
                    Err(TrySendError::Full(message)) => {
                        // Ordinary protocol traffic remains retained and
                        // backpressures stdin; only control frames may bypass
                        // this rendezvous through the separate bounded lane.
                        #[cfg(feature = "test-support")]
                        reader_test_barriers.record_workspace_fifo_reader_pending(&message);
                        if reader_sender.send(message).is_err() {
                            return Ok(());
                        }
                    }
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
        priority_receiver,
        StdioThreads { reader, writer },
    )
}

fn is_exit_notification(message: &Message) -> bool {
    matches!(message, Message::Notification(notification) if notification.method == "exit")
}

fn is_priority_control_message(message: &Message) -> bool {
    matches!(message,
        Message::Request(request) if request.method == "shutdown"
    ) || matches!(message,
        Message::Notification(notification)
            if notification.method == "exit" || notification.method == "$/cancelRequest"
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
    connection: &ProtocolConnection,
    test_barriers: TestBarrierConfig,
) -> Result<bool, Box<dyn Error + Send + Sync>> {
    let (initialize_id, initialize, options, initialize_value) = loop {
        let (initialize_id, initialize_value) = connection.initialize_start()?;
        let initialize: InitializeParams = match serde_json::from_value(initialize_value.clone()) {
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
        break (initialize_id, initialize, options, initialize_value);
    };
    let roots = workspace_roots(&initialize);
    let workspace_folders_supported = supports_workspace_folders(&initialize.capabilities);
    let configuration_pull_supported = supports_configuration(&initialize.capabilities);
    let watcher_registration_supported =
        supports_watched_file_registration(&initialize.capabilities);
    let relative_pattern_support = supports_relative_pattern(&initialize.capabilities);
    let client_features = client_features(&initialize.capabilities);
    let work_done_progress_supported = initialize
        .capabilities
        .window
        .as_ref()
        .and_then(|window| window.work_done_progress)
        .unwrap_or(false);
    let pull_diagnostics_supported = supports_pull_diagnostics(&initialize.capabilities);
    let pull_related_diagnostics_supported = supports_related_diagnostics(&initialize.capabilities);
    let diagnostic_refresh_supported =
        supports_diagnostic_refresh(&initialize.capabilities, &initialize_value);
    let workspace_diagnostics_supported = supports_workspace_diagnostic_reports(&initialize_value);
    let capabilities =
        server_capabilities(&initialize.capabilities, workspace_diagnostics_supported);

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

    let mut workspace = Workspace::new(roots, options.clone());
    let mut configuration = ConfigurationCoordinator::new(
        workspace.configuration_scope_uri(),
        options,
        configuration_pull_supported,
        pull_diagnostics_supported,
    );
    configuration.on_initialized(connection)?;
    let watcher_registration = watcher_registration_supported
        .then(|| register_file_watcher(connection, &workspace, relative_pattern_support))
        .transpose()?;
    let jobs =
        AnalysisJobs::with_test_barriers_and_progress(test_barriers, work_done_progress_supported);

    event_loop(
        connection,
        &mut workspace,
        workspace_folders_supported,
        client_features,
        pull_diagnostics_supported,
        pull_related_diagnostics_supported,
        diagnostic_refresh_supported,
        &mut configuration,
        watcher_registration,
        jobs,
    )
}

fn is_workspace_file_event_notification(method: &str) -> bool {
    matches!(
        method,
        "workspace/didChangeWatchedFiles"
            | "workspace/didCreateFiles"
            | "workspace/didDeleteFiles"
            | "workspace/didRenameFiles"
    )
}

struct WorkspaceFileNotificationWorker {
    receiver: Receiver<(
        Workspace,
        Result<DiagnosticNotificationEffect, String>,
        ReconciliationBudget,
    )>,
    cancellation: Arc<AtomicBool>,
    deadline_expired: Arc<AtomicBool>,
    deadline: Instant,
    join: Option<JoinHandle<()>>,
}

impl WorkspaceFileNotificationWorker {
    fn cancel_and_join(&mut self) -> thread::Result<()> {
        self.cancellation.store(true, Ordering::Release);
        self.join.take().map_or(Ok(()), JoinHandle::join)
    }
}

impl Drop for WorkspaceFileNotificationWorker {
    fn drop(&mut self) {
        self.cancellation.store(true, Ordering::Release);
        if let Some(join) = self.join.take() {
            // Preserve the only owned Workspace until worker teardown is complete.
            // This deliberately joins on exceptional exits rather than detaching
            // a state owner with no session/recovery path.
            let _ = join.join();
        }
    }
}

#[cfg(feature = "test-support")]
fn wait_at_diagnostic_work_test_barrier(budget: &ReconciliationBudget) -> Result<(), String> {
    use std::io::Write as _;

    let Ok(spec) = std::env::var("PASCAL_LSP_TEST_DIAGNOSTIC_WORK_BARRIER") else {
        return Ok(());
    };
    let Some((entered, release)) = spec.split_once('|') else {
        return Ok(());
    };
    let Ok(mut marker) = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(entered)
    else {
        return Ok(());
    };
    let _ = marker.write_all(b"x");
    drop(marker);
    while !std::path::Path::new(release).exists() {
        if budget.is_cancelled() {
            return Err(rename::CANCELLATION_MESSAGE.to_string());
        }
        thread::sleep(Duration::from_millis(10));
    }
    Ok(())
}

fn cancel_and_join_workspace_file_worker(
    worker: &mut Option<WorkspaceFileNotificationWorker>,
) -> thread::Result<()> {
    worker
        .take()
        .map_or(Ok(()), |mut worker| worker.cancel_and_join())
}

#[cfg(feature = "test-support")]
fn wait_at_workspace_file_worker_test_barrier(budget: &ReconciliationBudget) -> Result<(), String> {
    let Ok(spec) = std::env::var("PASCAL_LSP_TEST_WORKSPACE_FILE_WORKER_BARRIER") else {
        return Ok(());
    };
    let Some((entered, release)) = spec.split_once('|') else {
        return Ok(());
    };
    let mut marker = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(entered)
        .map_err(|error| format!("could not enter workspace file worker test barrier: {error}"))?;
    marker
        .write_all(b"x")
        .map_err(|error| format!("could not record workspace file worker barrier: {error}"))?;
    drop(marker);
    while !std::path::Path::new(release).exists() {
        if budget.is_cancelled() {
            return Err(rename::CANCELLATION_MESSAGE.to_string());
        }
        thread::sleep(Duration::from_millis(10));
    }
    Ok(())
}

#[cfg(not(feature = "test-support"))]
fn wait_at_workspace_file_worker_test_barrier(
    _budget: &ReconciliationBudget,
) -> Result<(), String> {
    Ok(())
}

fn spawn_workspace_file_notification(
    workspace: &mut Workspace,
    notification: Notification,
    workspace_folders_supported: bool,
    push_diagnostics_supported: bool,
) -> WorkspaceFileNotificationWorker {
    let (sender, receiver) = bounded(1);
    let cancellation = Arc::new(AtomicBool::new(false));
    let worker_cancellation = Arc::clone(&cancellation);
    let deadline_expired = Arc::new(AtomicBool::new(false));
    let deadline = Instant::now() + workspace_notification_deadline();
    let owned_workspace = std::mem::take(workspace);
    let join = thread::Builder::new()
        .name("PascalLspWorkspaceMutation".to_string())
        .spawn(move || {
            let mut workspace = owned_workspace;
            let budget = ReconciliationBudget::new(Arc::clone(&worker_cancellation));
            let mut result = wait_at_workspace_file_worker_test_barrier(&budget).and_then(|()| {
                handle_notification_with_control(
                    &UnusedProtocolSender,
                    &mut workspace,
                    notification,
                    workspace_folders_supported,
                    push_diagnostics_supported,
                    NotificationWorkControl {
                        cancel: Some(&worker_cancellation),
                        budget: Some(&budget),
                        defer_push_clears: false,
                    },
                )
            });
            if budget.is_exhausted() && !budget.is_cancelled() {
                workspace.invalidate_for_reconciliation_budget(&budget);
                let mut effect = DiagnosticNotificationEffect::default();
                effect.refresh_all_diagnostics();
                effect.discard_all_queued_diagnostics = true;
                if push_diagnostics_supported {
                    effect.clear_publication_cursor =
                        Some(workspace.take_all_diagnostic_publication_uris(None));
                }
                result = Ok(effect);
            }
            #[cfg(feature = "test-support")]
            if std::env::var_os("PASCAL_LSP_TEST_PANIC_FILE_WORKER").is_some() {
                panic!("injected file-notification worker panic");
            }
            #[cfg(feature = "test-support")]
            if let Some(path) = std::env::var_os("PASCAL_LSP_TEST_RECONCILIATION_WORK_RESULT") {
                let _ = std::fs::write(
                    path,
                    serde_json::to_vec(&budget.metrics(budget.is_exhausted())).unwrap_or_default(),
                );
            }
            let _ = sender.send((workspace, result, budget));
            #[cfg(feature = "test-support")]
            if let Some(path) = std::env::var_os("PASCAL_LSP_TEST_FILE_WORKER_COMPLETED") {
                let _ = std::fs::write(path, b"completed");
            }
        })
        .expect("failed to start serialized workspace mutation worker");
    WorkspaceFileNotificationWorker {
        receiver,
        cancellation,
        deadline_expired,
        deadline,
        join: Some(join),
    }
}

fn workspace_notification_deadline() -> Duration {
    #[cfg(feature = "test-support")]
    if let Some(milliseconds) = std::env::var("PASCAL_LSP_TEST_WORKSPACE_NOTIFICATION_DEADLINE_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
    {
        return Duration::from_millis(milliseconds.clamp(1, 30_000));
    }
    WORKSPACE_NOTIFICATION_DEADLINE
}

fn queue_workspace_message(
    queue: &mut VecDeque<(Message, usize)>,
    queued_bytes: &mut usize,
    overflow: &mut Option<Message>,
    message: Message,
) -> io::Result<()> {
    let bytes = serde_json::to_vec(&message)
        .map(|encoded| encoded.len())
        .unwrap_or(MAX_WORKSPACE_MUTATION_DEFERRED_BYTES + 1);
    if queue.len() >= MAX_CONFIGURATION_DEFERRED_MESSAGES
        || queued_bytes.saturating_add(bytes) > MAX_WORKSPACE_MUTATION_DEFERRED_BYTES
    {
        if overflow.is_some() {
            return Err(io::Error::other(
                "workspace mutation overflow slot occupied while reading continued",
            ));
        }
        #[cfg(feature = "test-support")]
        if std::env::var_os("PASCAL_LSP_TEST_DISABLE_WORKSPACE_FIFO_OVERFLOW")
            .is_some_and(|value| value == "1")
        {
            return Err(io::Error::other(
                "test hook disabled workspace mutation overflow admission",
            ));
        }
        *overflow = Some(message);
        return Ok(());
    }
    *queued_bytes = queued_bytes.saturating_add(bytes);
    queue.push_back((message, bytes));
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn event_loop(
    connection: &ProtocolConnection,
    workspace: &mut Workspace,
    workspace_folders_supported: bool,
    client_features: ClientFeatures,
    pull_diagnostics_supported: bool,
    pull_related_diagnostics_supported: bool,
    diagnostic_refresh_supported: bool,
    configuration: &mut ConfigurationCoordinator,
    mut watcher_registration: Option<FileWatcherRegistration>,
    mut jobs: AnalysisJobs,
) -> Result<bool, Box<dyn Error + Send + Sync>> {
    let mut shutdown_received = false;
    let mut deferred_configuration_messages = VecDeque::new();
    let mut deferred_workspace_messages = VecDeque::new();
    let mut deferred_workspace_message_bytes = 0usize;
    let mut deferred_workspace_overflow: Option<Message> = None;
    let mut file_notification_worker: Option<WorkspaceFileNotificationWorker> = None;
    let mut diagnostic_refresh = DiagnosticRefreshRequests::new(diagnostic_refresh_supported);
    let mut pending_diagnostic_clears = PendingDiagnosticClears::default();
    loop {
        connection.flush()?;
        let diagnostic_clears_were_pending = !pending_diagnostic_clears.is_empty();
        let mut diagnostic_publication_budget = DiagnosticPublicationTurnBudget::default();
        if !pull_diagnostics_supported
            && diagnostic_clears_were_pending
            && !connection.has_pending_output()
        {
            pending_diagnostic_clears.pump(connection, &mut diagnostic_publication_budget)?;
        }
        if let Some(worker) = file_notification_worker.as_ref() {
            match worker.receiver.try_recv() {
                Ok((mut completed_workspace, mut result, budget)) => {
                    let deadline_expired = worker.deadline_expired.load(Ordering::Acquire);
                    let mut worker = file_notification_worker
                        .take()
                        .expect("completed workspace file worker");
                    if worker.cancel_and_join().is_err() {
                        for (message, _) in deferred_workspace_messages.drain(..) {
                            if let Message::Request(request) = message {
                                send_error(
                                    connection,
                                    request.id,
                                    ErrorCode::RequestFailed,
                                    "workspace reconciliation worker panicked; retry after reconnect",
                                )?;
                            }
                        }
                        if let Some(Message::Request(request)) = deferred_workspace_overflow.take()
                        {
                            send_error(
                                connection,
                                request.id,
                                ErrorCode::RequestFailed,
                                "workspace reconciliation worker panicked; retry after reconnect",
                            )?;
                        }
                        jobs.shutdown_with_connection(connection)?;
                        return Ok(true);
                    }
                    if deadline_expired {
                        // Cancellation may have landed after one or more
                        // entries mutated the owned workspace. Discard its
                        // partial effect and use the same tombstone/rename-
                        // aware invalidation path as work-budget exhaustion.
                        eprintln!(
                            "pascal-lsp: workspace notification reconciliation exceeded its deadline; invalidating partial state"
                        );
                        completed_workspace.invalidate_for_reconciliation_budget(&budget);
                        let mut effect = DiagnosticNotificationEffect::default();
                        effect.refresh_all_diagnostics();
                        effect.discard_all_queued_diagnostics = true;
                        if !pull_diagnostics_supported {
                            effect.clear_publication_cursor = Some(
                                completed_workspace.take_all_diagnostic_publication_uris(None),
                            );
                        }
                        result = Ok(effect);
                    }
                    *workspace = completed_workspace;
                    match result {
                        Ok(effect) => {
                            if !pull_diagnostics_supported {
                                if effect.discard_all_queued_diagnostics {
                                    let scan = connection.discard_all_diagnostic_publications();
                                    diagnostic_publication_budget.publication_queue_scans =
                                        diagnostic_publication_budget
                                            .publication_queue_scans
                                            .saturating_add(1);
                                    diagnostic_publication_budget
                                        .publication_queue_messages_scanned =
                                        diagnostic_publication_budget
                                            .publication_queue_messages_scanned
                                            .saturating_add(scan.scanned_messages);
                                    diagnostic_publication_budget.publication_queue_bytes_scanned =
                                        diagnostic_publication_budget
                                            .publication_queue_bytes_scanned
                                            .saturating_add(scan.scanned_bytes);
                                    #[cfg(feature = "test-support")]
                                    write_publication_staling_test_metrics(0, 0, 0, 0, 0, scan);
                                }
                                if let Some(cursor) = effect.clear_publication_cursor {
                                    pending_diagnostic_clears
                                        .enqueue_cursor(cursor, effect.cleanup_rejected_uri);
                                }
                                if !effect.discard_all_queued_diagnostics {
                                    mark_refreshed_publication_roots_stale(
                                        connection,
                                        workspace,
                                        &effect.refresh,
                                        effect.refresh_all_diagnostics,
                                        Some(&effect.stale_publication_targets),
                                        &mut diagnostic_publication_budget,
                                        None,
                                    )?;
                                }
                            }
                            if pull_diagnostics_supported
                                && (effect.refresh_requested || !effect.refresh.is_empty())
                            {
                                diagnostic_refresh.request(connection)?;
                            }
                            if !pull_diagnostics_supported {
                                if effect.refresh_all_diagnostics {
                                    jobs.cancel_all_diagnostics_with_connection(Some(connection))
                                        .map_err(|error| -> Box<dyn Error + Send + Sync> {
                                            error.into()
                                        })?;
                                } else {
                                    jobs.cancel_diagnostics_for_with_connection(
                                        Some(connection),
                                        &effect.cancel,
                                    )
                                    .map_err(
                                        |error| -> Box<dyn Error + Send + Sync> { error.into() },
                                    )?;
                                    jobs.refresh_diagnostics_with_connection(
                                        connection,
                                        workspace,
                                        &effect.refresh,
                                    )
                                    .map_err(
                                        |error| -> Box<dyn Error + Send + Sync> { error.into() },
                                    )?;
                                }
                            }
                        }
                        Err(error) => {
                            eprintln!("pascal-lsp: notification handling failed: {error}")
                        }
                    }
                }
                Err(TryRecvError::Disconnected) => {
                    let mut worker = file_notification_worker
                        .take()
                        .expect("disconnected workspace file worker");
                    let _ = worker.cancel_and_join();
                    for (message, _) in deferred_workspace_messages.drain(..) {
                        if let Message::Request(request) = message {
                            send_error(
                                connection,
                                request.id,
                                ErrorCode::RequestFailed,
                                "workspace reconciliation worker failed; retry after reconnect",
                            )?;
                        }
                    }
                    if let Some(Message::Request(request)) = deferred_workspace_overflow.take() {
                        send_error(
                            connection,
                            request.id,
                            ErrorCode::RequestFailed,
                            "workspace reconciliation worker failed; retry after reconnect",
                        )?;
                    }
                    jobs.shutdown_with_connection(connection)?;
                    return Ok(true);
                }
                Err(TryRecvError::Empty) => {
                    if Instant::now() >= worker.deadline
                        && !worker.deadline_expired.swap(true, Ordering::AcqRel)
                    {
                        eprintln!(
                            "pascal-lsp: workspace notification reconciliation deadline elapsed; requesting cooperative cancellation"
                        );
                        worker.cancellation.store(true, Ordering::Release);
                    }
                }
            }
        }
        let workspace_busy = file_notification_worker.is_some();
        if !workspace_busy {
            if let Some(effect) = configuration.poll(workspace)? {
                if let Some(registration) = watcher_registration.as_mut() {
                    sync_file_watcher(connection, workspace, registration)?;
                }
                if pull_diagnostics_supported
                    && (effect.refresh_requested || !effect.refresh.is_empty())
                {
                    diagnostic_refresh.request(connection)?;
                }
                if !pull_diagnostics_supported {
                    mark_refreshed_publication_roots_stale(
                        connection,
                        workspace,
                        &effect.refresh,
                        false,
                        None,
                        &mut diagnostic_publication_budget,
                        None,
                    )?;
                    jobs.cancel_diagnostics_for_with_connection(Some(connection), &effect.cancel)
                        .map_err(|error| -> Box<dyn Error + Send + Sync> { error.into() })?;
                    jobs.refresh_diagnostics_with_connection(
                        connection,
                        workspace,
                        &effect.refresh,
                    )
                    .map_err(|error| -> Box<dyn Error + Send + Sync> { error.into() })?;
                }
            }
        }
        if !workspace_busy
            && !shutdown_received
            && !pull_diagnostics_supported
            && pending_diagnostic_clears.is_empty()
        {
            publish_due_diagnostics(connection, workspace, &mut jobs)?;
        }
        if !workspace_busy {
            jobs.poll_with_diagnostic_budget(
                connection,
                workspace,
                &mut diagnostic_publication_budget,
                pending_diagnostic_clears.is_empty(),
            )?;
        }
        if !pull_diagnostics_supported
            && !diagnostic_clears_were_pending
            && pending_diagnostic_clears.is_empty()
        {
            pump_pending_diagnostic_publications_with_budget(
                connection,
                workspace,
                &mut diagnostic_publication_budget,
            )?;
        }
        let output_pending = connection.has_pending_output();
        // Pull-owned diagnostics must not inherit the debounced push queue.
        // Those deadlines have no consumer in pull mode and would otherwise
        // turn every expired deadline into a zero-duration receive loop.
        let timeout = event_loop_receive_timeout(
            pull_diagnostics_supported,
            workspace.next_diagnostic_timeout(),
            jobs.is_empty(),
            output_pending,
            configuration.is_preparing() || workspace_busy,
        );
        let priority_message = connection.try_recv_priority();
        let deferred_workspace_message = if workspace_busy {
            None
        } else {
            deferred_workspace_messages
                .pop_front()
                .map(|(message, bytes)| {
                    deferred_workspace_message_bytes =
                        deferred_workspace_message_bytes.saturating_sub(bytes);
                    message
                })
                .or_else(|| deferred_workspace_overflow.take())
        };
        let message = if let Some(message) = priority_message {
            message
        } else if let Some(message) = deferred_workspace_message {
            message
        } else if workspace_busy && deferred_workspace_overflow.is_some() {
            thread::sleep(timeout);
            continue;
        } else if !workspace_busy && !configuration.is_preparing() {
            match deferred_configuration_messages.pop_front() {
                Some(DeferredConfigurationMessage::Request(deferred)) => {
                    if !deferred_request_is_current(workspace, configuration, &deferred) {
                        send_error(
                            connection,
                            deferred.request.id,
                            ErrorCode::RequestFailed,
                            "request became stale while configuration was prepared; retry the request",
                        )?;
                        jobs.pump_partial_deliveries(connection, workspace)?;
                        continue;
                    }
                    Message::Request(deferred.request)
                }
                Some(DeferredConfigurationMessage::Notification(notification)) => {
                    Message::Notification(notification)
                }
                None => match connection.receiver().recv_timeout(timeout) {
                    Ok(message) => message,
                    Err(RecvTimeoutError::Timeout) => {
                        if !workspace_busy
                            && !shutdown_received
                            && !pull_diagnostics_supported
                            && pending_diagnostic_clears.is_empty()
                        {
                            publish_due_diagnostics(connection, workspace, &mut jobs)?;
                        }
                        if !workspace_busy {
                            jobs.pump_partial_deliveries(connection, workspace)?;
                        }
                        continue;
                    }
                    Err(RecvTimeoutError::Disconnected) => {
                        let _ =
                            cancel_and_join_workspace_file_worker(&mut file_notification_worker);
                        jobs.shutdown();
                        configuration.shutdown();
                        diagnostic_refresh.shutdown();
                        return Ok(true);
                    }
                },
            }
        } else {
            match connection.receiver().recv_timeout(timeout) {
                Ok(message) => message,
                Err(RecvTimeoutError::Timeout) => {
                    if !workspace_busy
                        && !shutdown_received
                        && !pull_diagnostics_supported
                        && pending_diagnostic_clears.is_empty()
                    {
                        publish_due_diagnostics(connection, workspace, &mut jobs)?;
                    }
                    if !workspace_busy {
                        jobs.pump_partial_deliveries(connection, workspace)?;
                    }
                    continue;
                }
                Err(RecvTimeoutError::Disconnected) => {
                    let _ = cancel_and_join_workspace_file_worker(&mut file_notification_worker);
                    jobs.shutdown();
                    configuration.shutdown();
                    diagnostic_refresh.shutdown();
                    return Ok(true);
                }
            }
        };

        match message {
            Message::Request(request) if request.method == "shutdown" => {
                let _ = cancel_and_join_workspace_file_worker(&mut file_notification_worker);
                jobs.shutdown_with_connection(connection)?;
                configuration.shutdown();
                diagnostic_refresh.shutdown();
                for deferred in deferred_configuration_messages.drain(..) {
                    if let DeferredConfigurationMessage::Request(deferred) = deferred {
                        send_error(
                            connection,
                            deferred.request.id,
                            ErrorCode::RequestCanceled,
                            rename::CANCELLATION_MESSAGE,
                        )?;
                    }
                }
                for (deferred, _) in deferred_workspace_messages.drain(..) {
                    if let Message::Request(deferred) = deferred {
                        send_error(
                            connection,
                            deferred.id,
                            ErrorCode::RequestCanceled,
                            rename::CANCELLATION_MESSAGE,
                        )?;
                    }
                }
                if let Some(Message::Request(deferred)) = deferred_workspace_overflow.take() {
                    send_error(
                        connection,
                        deferred.id,
                        ErrorCode::RequestCanceled,
                        rename::CANCELLATION_MESSAGE,
                    )?;
                }
                deferred_workspace_message_bytes = 0;
                send_ok(connection, request.id, ())?;
                shutdown_received = true;
            }
            Message::Request(request) if file_notification_worker.is_some() => {
                queue_workspace_message(
                    &mut deferred_workspace_messages,
                    &mut deferred_workspace_message_bytes,
                    &mut deferred_workspace_overflow,
                    Message::Request(request),
                )?;
                #[cfg(feature = "test-support")]
                connection.record_workspace_fifo_state(
                    deferred_workspace_messages.len(),
                    deferred_workspace_message_bytes,
                    deferred_workspace_overflow.as_ref(),
                );
            }
            Message::Request(request) => {
                if shutdown_received {
                    send_error(
                        connection,
                        request.id,
                        ErrorCode::InvalidRequest,
                        "request received after shutdown",
                    )?;
                } else if configuration.is_preparing()
                    && request_requires_configuration(&request.method)
                {
                    let deferred_request_count = deferred_configuration_messages
                        .iter()
                        .filter(|deferred| {
                            matches!(deferred, DeferredConfigurationMessage::Request(_))
                        })
                        .count();
                    if deferred_configuration_messages.len() >= MAX_CONFIGURATION_DEFERRED_MESSAGES
                        || deferred_request_count >= MAX_CONFIGURATION_DEFERRED_REQUESTS
                    {
                        send_error(
                            connection,
                            request.id,
                            ErrorCode::ServerCancelled,
                            CONFIGURATION_REQUEST_RETRY_MESSAGE,
                        )?;
                    } else if deferred_configuration_messages.iter().any(|deferred| {
                        matches!(
                            deferred,
                            DeferredConfigurationMessage::Request(deferred)
                                if deferred.request.id == request.id
                        )
                    }) {
                        send_error(
                            connection,
                            request.id,
                            ErrorCode::InvalidRequest,
                            "analysis request ID is already in use",
                        )?;
                    } else {
                        let preceding_document_notification =
                            deferred_configuration_messages.iter().any(|deferred| {
                                matches!(
                                    deferred,
                                    DeferredConfigurationMessage::Notification(notification)
                                        if notification_may_change_document(&notification.method)
                                )
                            });
                        let preceding_configuration_notification =
                            deferred_configuration_messages.iter().any(|deferred| {
                                matches!(
                                    deferred,
                                    DeferredConfigurationMessage::Notification(notification)
                                        if notification_may_change_configuration(
                                            &notification.method
                                        )
                                )
                            });
                        deferred_configuration_messages.push_back(
                            DeferredConfigurationMessage::Request(deferred_configuration_request(
                                request,
                                workspace,
                                configuration,
                                preceding_document_notification,
                                preceding_configuration_notification,
                            )),
                        );
                    }
                } else {
                    let source_generation = workspace.source_generation();
                    let configuration_generation = workspace.configuration_generation();
                    handle_request(
                        connection,
                        workspace,
                        request,
                        client_features,
                        pull_diagnostics_supported,
                        pull_related_diagnostics_supported,
                        &mut jobs,
                    )?;
                    if source_generation != workspace.source_generation()
                        || configuration_generation != workspace.configuration_generation()
                    {
                        let open_documents = workspace.open_document_uris();
                        if pull_diagnostics_supported {
                            diagnostic_refresh.request(connection)?;
                        }
                        if !pull_diagnostics_supported {
                            mark_refreshed_publication_roots_stale(
                                connection,
                                workspace,
                                &open_documents,
                                false,
                                None,
                                &mut diagnostic_publication_budget,
                                None,
                            )?;
                            jobs.refresh_diagnostics_with_connection(
                                connection,
                                workspace,
                                &open_documents,
                            )
                            .map_err(|error| -> Box<dyn Error + Send + Sync> { error.into() })?;
                        }
                    }
                    if let Some(registration) = watcher_registration.as_mut() {
                        sync_file_watcher(connection, workspace, registration)?;
                    }
                }
            }
            Message::Notification(notification) if notification.method == "exit" => {
                let _ = cancel_and_join_workspace_file_worker(&mut file_notification_worker);
                jobs.shutdown_with_connection(connection)?;
                configuration.shutdown();
                diagnostic_refresh.shutdown();
                return Ok(shutdown_received);
            }
            Message::Notification(notification)
                if configuration.is_preparing()
                    && notification_requires_configuration_ordering(&notification.method) =>
            {
                let coalesce = configuration.can_coalesce_configuration_notifications()
                    && notification.method == "workspace/didChangeConfiguration"
                    && matches!(
                        deferred_configuration_messages.back(),
                        Some(DeferredConfigurationMessage::Notification(previous))
                            if previous.method == "workspace/didChangeConfiguration"
                    );
                if coalesce {
                    deferred_configuration_messages.pop_back();
                } else if deferred_configuration_messages.len()
                    >= MAX_CONFIGURATION_DEFERRED_MESSAGES
                    && !reject_deferred_configuration_request(
                        connection,
                        &mut deferred_configuration_messages,
                    )?
                {
                    return Err(
                        "configuration transition queue is full; cannot safely defer a state-changing notification"
                            .into(),
                    );
                }
                deferred_configuration_messages
                    .push_back(DeferredConfigurationMessage::Notification(notification));
            }
            Message::Notification(notification) => {
                if notification.method == "window/workDoneProgress/cancel" {
                    if let Ok(params) =
                        serde_json::from_value::<WorkDoneProgressCancelParams>(notification.params)
                    {
                        jobs.cancel_progress(connection, &params.token)
                            .map_err(|error| -> Box<dyn Error + Send + Sync> { error.into() })?;
                    }
                    if file_notification_worker.is_none() {
                        jobs.pump_partial_deliveries(connection, workspace)?;
                    }
                    continue;
                }
                if notification.method == "$/cancelRequest" {
                    if let Ok(id) = serde_json::from_value::<RequestId>(
                        notification
                            .params
                            .get("id")
                            .cloned()
                            .unwrap_or(Value::Null),
                    ) {
                        if let Some(index) =
                            deferred_configuration_messages.iter().position(|deferred| {
                                matches!(
                                    deferred,
                                    DeferredConfigurationMessage::Request(request)
                                        if request.request.id == id
                                )
                            })
                        {
                            let request = deferred_configuration_messages
                                .remove(index)
                                .expect("deferred configuration request");
                            let DeferredConfigurationMessage::Request(request) = request else {
                                unreachable!("deferred request predicate");
                            };
                            send_error(
                                connection,
                                request.request.id,
                                ErrorCode::RequestCanceled,
                                rename::CANCELLATION_MESSAGE,
                            )?;
                        } else if let Some(index) = deferred_workspace_messages.iter().position(
                            |(message, _)| {
                                matches!(message, Message::Request(request) if request.id == id)
                            },
                        ) {
                            let (message, bytes) = deferred_workspace_messages
                                .remove(index)
                                .expect("deferred workspace request index");
                            deferred_workspace_message_bytes =
                                deferred_workspace_message_bytes.saturating_sub(bytes);
                            let Message::Request(request) = message else {
                                unreachable!("deferred workspace request predicate");
                            };
                            send_error(
                                connection,
                                request.id,
                                ErrorCode::RequestCanceled,
                                rename::CANCELLATION_MESSAGE,
                            )?;
                        } else if matches!(
                            deferred_workspace_overflow.as_ref(),
                            Some(Message::Request(request)) if request.id == id
                        ) {
                            deferred_workspace_overflow = None;
                            send_error(
                                connection,
                                id,
                                ErrorCode::RequestCanceled,
                                rename::CANCELLATION_MESSAGE,
                            )?;
                        } else {
                            jobs.cancel(connection, &id).map_err(
                                |error| -> Box<dyn Error + Send + Sync> { error.into() },
                            )?;
                        }
                    }
                    if file_notification_worker.is_none() {
                        jobs.pump_partial_deliveries(connection, workspace)?;
                    }
                    continue;
                }
                if file_notification_worker.is_some() {
                    queue_workspace_message(
                        &mut deferred_workspace_messages,
                        &mut deferred_workspace_message_bytes,
                        &mut deferred_workspace_overflow,
                        Message::Notification(notification),
                    )?;
                    #[cfg(feature = "test-support")]
                    connection.record_workspace_fifo_state(
                        deferred_workspace_messages.len(),
                        deferred_workspace_message_bytes,
                        deferred_workspace_overflow.as_ref(),
                    );
                    continue;
                }
                let notification_method = notification.method.clone();
                let result = if notification_method == "initialized" {
                    configuration.on_initialized(connection)?;
                    Ok(DiagnosticNotificationEffect::default())
                } else if notification_method == "workspace/didChangeConfiguration" {
                    configuration
                        .handle_notification(connection, workspace, &notification)
                        .map_err(|error| error.to_string())
                } else {
                    if is_workspace_file_event_notification(&notification_method) {
                        file_notification_worker = Some(spawn_workspace_file_notification(
                            workspace,
                            notification,
                            workspace_folders_supported,
                            !pull_diagnostics_supported,
                        ));
                        continue;
                    }
                    let refresh_configuration =
                        notification_method == "workspace/didChangeWorkspaceFolders";
                    let result = handle_notification_with_cancel(
                        connection,
                        workspace,
                        notification,
                        workspace_folders_supported,
                        !pull_diagnostics_supported,
                        None,
                        !pending_diagnostic_clears.is_empty(),
                    );
                    if refresh_configuration && result.is_ok() {
                        configuration
                            .update_scope(workspace.configuration_scope_uri(), workspace)
                            .map_err(|error| error.to_string())?;
                        configuration
                            .request_refresh(connection)
                            .map_err(|error| error.to_string())?;
                    }
                    result
                };
                match result {
                    Ok(effect) => {
                        if !pull_diagnostics_supported {
                            if let Some(cursor) = effect.clear_publication_cursor {
                                pending_diagnostic_clears
                                    .enqueue_cursor(cursor, effect.cleanup_rejected_uri);
                            }
                        }
                        if let Some(registration) = watcher_registration.as_mut() {
                            sync_file_watcher(connection, workspace, registration)?;
                        }
                        if pull_diagnostics_supported
                            && (effect.refresh_requested || !effect.refresh.is_empty())
                        {
                            diagnostic_refresh.request(connection)?;
                        }
                        if !pull_diagnostics_supported {
                            if effect.discard_all_queued_diagnostics {
                                let scan = connection.discard_all_diagnostic_publications();
                                diagnostic_publication_budget.publication_queue_scans =
                                    diagnostic_publication_budget
                                        .publication_queue_scans
                                        .saturating_add(1);
                                diagnostic_publication_budget.publication_queue_messages_scanned =
                                    diagnostic_publication_budget
                                        .publication_queue_messages_scanned
                                        .saturating_add(scan.scanned_messages);
                                diagnostic_publication_budget.publication_queue_bytes_scanned =
                                    diagnostic_publication_budget
                                        .publication_queue_bytes_scanned
                                        .saturating_add(scan.scanned_bytes);
                                #[cfg(feature = "test-support")]
                                write_publication_staling_test_metrics(0, 0, 0, 0, 0, scan);
                            } else {
                                mark_refreshed_publication_roots_stale(
                                    connection,
                                    workspace,
                                    &effect.refresh,
                                    effect.refresh_all_diagnostics,
                                    Some(&effect.stale_publication_targets),
                                    &mut diagnostic_publication_budget,
                                    None,
                                )?;
                            }
                            jobs.cancel_diagnostics_for_with_connection(
                                Some(connection),
                                &effect.cancel,
                            )
                            .map_err(|error| -> Box<dyn Error + Send + Sync> { error.into() })?;
                            jobs.refresh_diagnostics_with_connection(
                                connection,
                                workspace,
                                &effect.refresh,
                            )
                            .map_err(|error| -> Box<dyn Error + Send + Sync> { error.into() })?;
                        }
                    }
                    Err(error) => {
                        eprintln!("pascal-lsp: notification handling failed: {error}");
                    }
                }
            }
            Message::Response(response) => {
                if file_notification_worker.is_some() {
                    queue_workspace_message(
                        &mut deferred_workspace_messages,
                        &mut deferred_workspace_message_bytes,
                        &mut deferred_workspace_overflow,
                        Message::Response(response),
                    )?;
                    #[cfg(feature = "test-support")]
                    connection.record_workspace_fifo_state(
                        deferred_workspace_messages.len(),
                        deferred_workspace_message_bytes,
                        deferred_workspace_overflow.as_ref(),
                    );
                    continue;
                }
                if diagnostic_refresh.handle_response(connection, &response)? {
                    // Refresh responses are deliberately non-blocking. A
                    // pending coalesced refresh, if any, was sent by the
                    // coordinator above.
                } else if let Some(effect) =
                    configuration.handle_response(connection, workspace, &response)?
                {
                    if let Some(registration) = watcher_registration.as_mut() {
                        sync_file_watcher(connection, workspace, registration)?;
                    }
                    if pull_diagnostics_supported
                        && (effect.refresh_requested || !effect.refresh.is_empty())
                    {
                        diagnostic_refresh.request(connection)?;
                    }
                    if !pull_diagnostics_supported {
                        mark_refreshed_publication_roots_stale(
                            connection,
                            workspace,
                            &effect.refresh,
                            effect.refresh_all_diagnostics,
                            Some(&effect.stale_publication_targets),
                            &mut diagnostic_publication_budget,
                            None,
                        )?;
                        jobs.cancel_diagnostics_for_with_connection(
                            Some(connection),
                            &effect.cancel,
                        )
                        .map_err(|error| -> Box<dyn Error + Send + Sync> { error.into() })?;
                        jobs.refresh_diagnostics_with_connection(
                            connection,
                            workspace,
                            &effect.refresh,
                        )
                        .map_err(|error| -> Box<dyn Error + Send + Sync> { error.into() })?;
                    }
                } else if jobs
                    .handle_progress_response(connection, &response)
                    .map_err(|error| -> Box<dyn Error + Send + Sync> { error.into() })?
                {
                    // Progress-create responses are intentionally handled
                    // before watcher/error logging.  Their IDs use a
                    // disjoint prefix from Task20 configuration IDs.
                } else {
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
        if file_notification_worker.is_none() {
            jobs.pump_partial_deliveries(connection, workspace)?;
        }
    }
}

fn event_loop_receive_timeout(
    pull_diagnostics_supported: bool,
    diagnostic_timeout: Option<Duration>,
    jobs_empty: bool,
    output_pending: bool,
    configuration_preparing: bool,
) -> Duration {
    let diagnostic_timeout = (!pull_diagnostics_supported)
        .then_some(diagnostic_timeout)
        .flatten()
        .unwrap_or(Duration::from_secs(86_400));
    diagnostic_timeout
        .min(if jobs_empty && !output_pending {
            Duration::from_secs(86_400)
        } else {
            ANALYSIS_POLL_INTERVAL
        })
        .min(if configuration_preparing {
            ANALYSIS_POLL_INTERVAL
        } else {
            Duration::from_secs(86_400)
        })
}

fn start_analysis(
    connection: &dyn ProtocolSender,
    workspace: &Workspace,
    jobs: &mut AnalysisJobs,
    id: RequestId,
    request: AnalysisRequest,
    features: ClientFeatures,
    work_done_token: Option<ProgressToken>,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    start_analysis_with_partial(
        connection,
        workspace,
        jobs,
        id,
        request,
        features,
        AnalysisProgressTokens {
            work_done: work_done_token,
            partial_result: None,
        },
    )
}

fn start_analysis_with_partial(
    connection: &dyn ProtocolSender,
    workspace: &Workspace,
    jobs: &mut AnalysisJobs,
    id: RequestId,
    request: AnalysisRequest,
    features: ClientFeatures,
    tokens: AnalysisProgressTokens,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    if let Err(error) = jobs.enqueue_client_with_partial(
        id.clone(),
        request,
        workspace,
        features,
        tokens,
        Some(connection),
    ) {
        send_error(connection, id, ErrorCode::RequestFailed, error)?;
    }
    Ok(())
}

fn request_work_done_token(request: &Request) -> Result<Option<ProgressToken>, String> {
    let Some(value) = request.params.get("workDoneToken") else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    serde_json::from_value(value.clone())
        .map(Some)
        .map_err(|error| format!("workDoneToken must be a string or integer: {error}"))
}

fn request_partial_result_token(request: &Request) -> Result<Option<ProgressToken>, String> {
    let Some(value) = request.params.get("partialResultToken") else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    serde_json::from_value(value.clone())
        .map(Some)
        .map_err(|error| format!("partialResultToken must be a string or integer: {error}"))
}

fn request_requires_configuration(method: &str) -> bool {
    matches!(
        method,
        "workspace/diagnostic"
            | "textDocument/diagnostic"
            | "pascal/projectContext"
            | "pascal/selectProject"
            | "textDocument/hover"
            | "textDocument/completion"
            | "completionItem/resolve"
            | "textDocument/signatureHelp"
            | "textDocument/typeDefinition"
            | "textDocument/documentSymbol"
            | "workspace/symbol"
            | "textDocument/references"
            | "textDocument/documentHighlight"
            | "textDocument/selectionRange"
            | "textDocument/semanticTokens/full"
            | "textDocument/semanticTokens/range"
            | "textDocument/foldingRange"
            | "textDocument/prepareRename"
            | "textDocument/rename"
            | "textDocument/codeAction"
            | "codeAction/resolve"
            | "textDocument/declaration"
            | "textDocument/definition"
            | "textDocument/implementation"
            | "textDocument/formatting"
            | "textDocument/rangeFormatting"
            | "textDocument/onTypeFormatting"
            | "workspace/willCreateFiles"
            | "workspace/willRenameFiles"
            | "workspace/willDeleteFiles"
    )
}

fn notification_requires_configuration_ordering(method: &str) -> bool {
    matches!(
        method,
        "workspace/didChangeConfiguration"
            | "textDocument/didOpen"
            | "textDocument/didChange"
            | "textDocument/didSave"
            | "textDocument/didClose"
            | "workspace/didChangeWatchedFiles"
            | "workspace/didCreateFiles"
            | "workspace/didRenameFiles"
            | "workspace/didDeleteFiles"
            | "workspace/didChangeWorkspaceFolders"
    )
}

fn notification_may_change_document(method: &str) -> bool {
    matches!(
        method,
        "textDocument/didOpen"
            | "textDocument/didChange"
            | "textDocument/didSave"
            | "textDocument/didClose"
            | "workspace/didChangeWatchedFiles"
            | "workspace/didCreateFiles"
            | "workspace/didRenameFiles"
            | "workspace/didDeleteFiles"
            | "workspace/didChangeWorkspaceFolders"
    )
}

fn notification_may_change_configuration(method: &str) -> bool {
    matches!(
        method,
        "workspace/didChangeConfiguration" | "workspace/didChangeWorkspaceFolders"
    )
}

fn reject_deferred_configuration_request(
    connection: &dyn ProtocolSender,
    messages: &mut VecDeque<DeferredConfigurationMessage>,
) -> Result<bool, Box<dyn Error + Send + Sync>> {
    let Some(index) = messages
        .iter()
        .rposition(|message| matches!(message, DeferredConfigurationMessage::Request(_)))
    else {
        return Ok(false);
    };
    let Some(DeferredConfigurationMessage::Request(deferred)) = messages.remove(index) else {
        unreachable!("deferred request predicate");
    };
    send_error(
        connection,
        deferred.request.id,
        ErrorCode::ServerCancelled,
        CONFIGURATION_REQUEST_RETRY_MESSAGE,
    )?;
    Ok(true)
}

fn request_document_uri(request: &Request) -> Option<Url> {
    request
        .params
        .get("textDocument")
        .and_then(|document| document.get("uri"))
        .and_then(Value::as_str)
        .and_then(|uri| uri.parse().ok())
}

fn deferred_configuration_request(
    request: Request,
    workspace: &Workspace,
    configuration: &ConfigurationCoordinator,
    preceding_document_notification: bool,
    preceding_configuration_notification: bool,
) -> DeferredConfigurationRequest {
    let document = request_document_uri(&request).map(|uri| {
        let (version, generation) = workspace.document_identity(&uri);
        DeferredDocumentIdentity {
            version,
            generation,
            uri,
        }
    });
    DeferredConfigurationRequest {
        request,
        configuration_revision: configuration.deferred_request_revision(),
        document,
        preceding_document_notification,
        preceding_configuration_notification,
    }
}

fn deferred_request_is_current(
    workspace: &Workspace,
    configuration: &ConfigurationCoordinator,
    deferred: &DeferredConfigurationRequest,
) -> bool {
    let configuration_current = configuration.deferred_request_revision()
        == deferred.configuration_revision
        || deferred.preceding_configuration_notification;
    let document_current = deferred.document.as_ref().is_none_or(|document| {
        let current = workspace.document_identity(&document.uri);
        current == (document.version, document.generation)
            || deferred.preceding_document_notification
    });
    configuration_current && document_current
}

fn handle_request(
    connection: &dyn ProtocolSender,
    workspace: &mut Workspace,
    request: Request,
    client_features: ClientFeatures,
    pull_diagnostics_supported: bool,
    pull_related_diagnostics_supported: bool,
    jobs: &mut AnalysisJobs,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    if matches!(
        request.method.as_str(),
        "workspace/willCreateFiles" | "workspace/willDeleteFiles"
    ) {
        // These operations have no safe pre-operation source edits to offer.
        connection.send_result(Message::Response(Response::new_ok(request.id, Value::Null)))?;
        return Ok(());
    }
    if workspace.analysis_admission_fenced() {
        send_error(
            connection,
            request.id,
            ErrorCode::RequestFailed,
            OPEN_ADMISSION_FENCE_MESSAGE,
        )?;
        return Ok(());
    }
    let work_done_token = match request_work_done_token(&request) {
        Ok(token) => token,
        Err(error) => {
            send_error(
                connection,
                request.id.clone(),
                ErrorCode::InvalidParams,
                error,
            )?;
            return Ok(());
        }
    };
    let partial_result_token = if matches!(
        request.method.as_str(),
        "workspace/symbol" | "textDocument/references" | "workspace/diagnostic"
    ) {
        match request_partial_result_token(&request) {
            Ok(token) => token,
            Err(error) => {
                send_error(
                    connection,
                    request.id.clone(),
                    ErrorCode::InvalidParams,
                    error,
                )?;
                return Ok(());
            }
        }
    } else {
        None
    };
    if request.method == "workspace/willRenameFiles" {
        if !client_features.will_rename_files || !client_features.document_changes {
            send_error(
                connection,
                request.id,
                ErrorCode::RequestFailed,
                "unit file rename requires client willRename and versioned documentChanges support",
            )?;
            return Ok(());
        }
        let Some(files) = request.params.get("files").and_then(Value::as_array) else {
            send_error(
                connection,
                request.id,
                ErrorCode::InvalidParams,
                "files must be an array",
            )?;
            return Ok(());
        };
        if files.len() != 1 {
            send_error(
                connection,
                request.id,
                ErrorCode::RequestFailed,
                "atomic multi-file and directory unit renames are unsupported",
            )?;
            return Ok(());
        }
        let Some(old_uri) = files[0]
            .get("oldUri")
            .cloned()
            .and_then(|value| serde_json::from_value::<Url>(value).ok())
        else {
            send_error(
                connection,
                request.id,
                ErrorCode::InvalidParams,
                "oldUri must be a valid URI",
            )?;
            return Ok(());
        };
        let Some(new_uri) = files[0]
            .get("newUri")
            .cloned()
            .and_then(|value| serde_json::from_value::<Url>(value).ok())
        else {
            send_error(
                connection,
                request.id,
                ErrorCode::InvalidParams,
                "newUri must be a valid URI",
            )?;
            return Ok(());
        };
        let old_uri = canonical_file_uri(&old_uri);
        let new_uri = canonical_file_uri(&new_uri);
        let (position, new_name) = match workspace.unit_rename_position(&old_uri, &new_uri) {
            Ok(value) => value,
            Err(error) => {
                send_error(connection, request.id, ErrorCode::RequestFailed, error)?;
                return Ok(());
            }
        };
        start_analysis(
            connection,
            workspace,
            jobs,
            request.id,
            AnalysisRequest::Rename {
                uri: old_uri,
                position,
                new_name,
                new_uri: Some(new_uri),
            },
            client_features,
            work_done_token,
        )?;
        return Ok(());
    }
    match request.method.as_str() {
        "workspace/diagnostic" => {
            if !pull_diagnostics_supported {
                send_error(
                    connection,
                    request.id,
                    ErrorCode::MethodNotFound,
                    "workspace/diagnostic was not negotiated by this client",
                )?;
                return Ok(());
            }
            let id = request.id.clone();
            let params: WorkspaceDiagnosticParams = match parse_params(&request) {
                Ok(params) => params,
                Err(error) => {
                    send_error(connection, id, ErrorCode::InvalidParams, error)?;
                    return Ok(());
                }
            };
            if params
                .identifier
                .as_deref()
                .is_some_and(|identifier| identifier != SERVER_NAME)
            {
                send_error(
                    connection,
                    id,
                    ErrorCode::InvalidParams,
                    "unsupported diagnostic provider identifier",
                )?;
                return Ok(());
            }
            let previous_result_ids = params
                .previous_result_ids
                .into_iter()
                .map(|previous| (canonical_file_uri(&previous.uri), previous.value))
                .collect();
            start_analysis_with_partial(
                connection,
                workspace,
                jobs,
                request.id,
                AnalysisRequest::WorkspaceDiagnostics {
                    previous_result_ids,
                },
                client_features,
                AnalysisProgressTokens {
                    work_done: params.work_done_progress_params.work_done_token,
                    partial_result: partial_result_token,
                },
            )?;
        }
        "textDocument/diagnostic" => {
            #[cfg(feature = "test-support")]
            let test_request_override =
                std::env::var_os("PASCAL_LSP_TEST_ALLOW_PULL_DIAGNOSTICS_REQUESTS").is_some();
            #[cfg(not(feature = "test-support"))]
            let test_request_override = false;
            if !pull_diagnostics_supported && !test_request_override {
                send_error(
                    connection,
                    request.id,
                    ErrorCode::MethodNotFound,
                    "textDocument/diagnostic was not negotiated by this client",
                )?;
                return Ok(());
            }
            let id = request.id.clone();
            let params: DocumentDiagnosticParams = match parse_params(&request) {
                Ok(params) => params,
                Err(error) => {
                    send_error(connection, id, ErrorCode::InvalidParams, error)?;
                    return Ok(());
                }
            };
            if params
                .identifier
                .as_deref()
                .is_some_and(|identifier| identifier != SERVER_NAME)
            {
                send_error(
                    connection,
                    id,
                    ErrorCode::InvalidParams,
                    "unsupported diagnostic provider identifier",
                )?;
                return Ok(());
            }
            start_analysis(
                connection,
                workspace,
                jobs,
                request.id,
                AnalysisRequest::DocumentDiagnostics {
                    uri: canonical_file_uri(&params.text_document.uri),
                    previous_result_id: params.previous_result_id,
                    related_document_support: pull_related_diagnostics_supported,
                },
                client_features,
                params.work_done_progress_params.work_done_token,
            )?;
        }
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
                work_done_token.clone(),
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
                    snippet_support: client_features.completion_snippet_support,
                    resolve_documentation: client_features.completion_resolve_documentation,
                    resolve_detail: client_features.completion_resolve_detail,
                },
                client_features,
                work_done_token.clone(),
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
                work_done_token.clone(),
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
                work_done_token.clone(),
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
                work_done_token.clone(),
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
                work_done_token.clone(),
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
            start_analysis_with_partial(
                connection,
                workspace,
                jobs,
                request.id,
                AnalysisRequest::WorkspaceSymbols {
                    query: params.query,
                },
                client_features,
                AnalysisProgressTokens {
                    work_done: work_done_token.clone(),
                    partial_result: partial_result_token.clone(),
                },
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
            start_analysis_with_partial(
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
                AnalysisProgressTokens {
                    work_done: work_done_token.clone(),
                    partial_result: partial_result_token.clone(),
                },
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
                work_done_token.clone(),
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
                work_done_token.clone(),
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
                work_done_token.clone(),
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
                work_done_token.clone(),
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
                work_done_token.clone(),
            )?;
        }
        "textDocument/inlayHint" => {
            let id = request.id.clone();
            let params: lsp_types::InlayHintParams = match parse_params(&request) {
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
                AnalysisRequest::InlayHints {
                    uri: canonical_file_uri(&params.text_document.uri),
                    range: params.range,
                },
                client_features,
                work_done_token.clone(),
            )?;
        }
        "textDocument/prepareCallHierarchy" => {
            let id = request.id.clone();
            let params: lsp_types::CallHierarchyPrepareParams = match parse_params(&request) {
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
                AnalysisRequest::PrepareCallHierarchy {
                    uri: canonical_file_uri(
                        &params.text_document_position_params.text_document.uri,
                    ),
                    position: params.text_document_position_params.position,
                },
                client_features,
                work_done_token.clone(),
            )?;
        }
        "callHierarchy/incomingCalls" => {
            let id = request.id.clone();
            let params: lsp_types::CallHierarchyIncomingCallsParams = match parse_params(&request) {
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
                AnalysisRequest::IncomingCalls { item: params.item },
                client_features,
                work_done_token.clone(),
            )?;
        }
        "callHierarchy/outgoingCalls" => {
            let id = request.id.clone();
            let params: lsp_types::CallHierarchyOutgoingCallsParams = match parse_params(&request) {
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
                AnalysisRequest::OutgoingCalls { item: params.item },
                client_features,
                work_done_token.clone(),
            )?;
        }
        "textDocument/prepareTypeHierarchy" => {
            let id = request.id.clone();
            let params: lsp_types::TypeHierarchyPrepareParams = match parse_params(&request) {
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
                AnalysisRequest::PrepareTypeHierarchy {
                    uri: canonical_file_uri(
                        &params.text_document_position_params.text_document.uri,
                    ),
                    position: params.text_document_position_params.position,
                },
                client_features,
                work_done_token.clone(),
            )?;
        }
        "typeHierarchy/supertypes" => {
            let id = request.id.clone();
            let params: lsp_types::TypeHierarchySupertypesParams = match parse_params(&request) {
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
                AnalysisRequest::TypeHierarchySupertypes { item: params.item },
                client_features,
                work_done_token.clone(),
            )?;
        }
        "typeHierarchy/subtypes" => {
            let id = request.id.clone();
            let params: lsp_types::TypeHierarchySubtypesParams = match parse_params(&request) {
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
                AnalysisRequest::TypeHierarchySubtypes { item: params.item },
                client_features,
                work_done_token.clone(),
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
                work_done_token.clone(),
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
                    new_uri: None,
                },
                client_features,
                work_done_token.clone(),
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
                work_done_token.clone(),
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
                work_done_token.clone(),
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
                work_done_token.clone(),
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
                    range: None,
                    on_type_cursor: None,
                    tab_size: params.options.tab_size,
                    insert_spaces: params.options.insert_spaces,
                },
                client_features,
                work_done_token.clone(),
            )?;
        }
        "textDocument/documentLink" => {
            let id = request.id.clone();
            let params: lsp_types::DocumentLinkParams = match parse_params(&request) {
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
                AnalysisRequest::DocumentLinks {
                    uri: canonical_file_uri(&params.text_document.uri),
                },
                client_features,
                work_done_token.clone(),
            )?;
        }
        "textDocument/rangeFormatting" => {
            let id = request.id.clone();
            let params: lsp_types::DocumentRangeFormattingParams = match parse_params(&request) {
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
                    range: Some(params.range),
                    on_type_cursor: None,
                    tab_size: params.options.tab_size,
                    insert_spaces: params.options.insert_spaces,
                },
                client_features,
                work_done_token.clone(),
            )?;
        }
        "textDocument/onTypeFormatting" => {
            let id = request.id.clone();
            let params: lsp_types::DocumentOnTypeFormattingParams = match parse_params(&request) {
                Ok(params) => params,
                Err(error) => {
                    send_error(connection, id, ErrorCode::InvalidParams, error)?;
                    return Ok(());
                }
            };
            if params.ch != ";" {
                send_error(
                    connection,
                    id,
                    ErrorCode::InvalidParams,
                    "unsupported on-type formatting trigger".to_string(),
                )?;
                return Ok(());
            }
            let position = params.text_document_position.position;
            let range = lsp_types::Range::new(Position::new(position.line, 0), position);
            start_analysis(
                connection,
                workspace,
                jobs,
                request.id,
                AnalysisRequest::Formatting {
                    uri: canonical_file_uri(&params.text_document_position.text_document.uri),
                    range: Some(range),
                    on_type_cursor: Some(position),
                    tab_size: params.options.tab_size,
                    insert_spaces: params.options.insert_spaces,
                },
                client_features,
                work_done_token.clone(),
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

fn add_file_operation_uri_bytes(total: &mut usize, uri: &Url) -> Result<(), String> {
    let next = total
        .checked_add(uri.as_str().len())
        .ok_or_else(|| "file operation URI byte count overflow".to_string())?;
    if next > MAX_FILE_OPERATION_BATCH_URI_BYTES {
        return Err(format!(
            "file operation URI bytes exceed the {MAX_FILE_OPERATION_BATCH_URI_BYTES}-byte batch limit"
        ));
    }
    *total = next;
    Ok(())
}

fn invalidate_ambiguous_file_notification(
    workspace: &mut Workspace,
    budget: Option<&ReconciliationBudget>,
    endpoints: impl IntoIterator<Item = Url>,
    push_diagnostics_supported: bool,
) -> DiagnosticNotificationEffect {
    let fallback_budget = ReconciliationBudget::new(Arc::new(AtomicBool::new(false)));
    let budget = budget.unwrap_or(&fallback_budget);
    let mut unique_endpoints = HashSet::new();
    for uri in endpoints {
        if unique_endpoints.insert(uri.clone()) {
            // For an ambiguous event, no endpoint is allowed to retain a
            // positive disk observation. Recovery keeps these as tombstones
            // until a later verified file event supersedes them.
            budget.record_file_event(uri.clone(), FileChange::Deleted);
            budget.record_rename_endpoint(uri);
        }
    }
    workspace.invalidate_for_reconciliation_budget(budget);
    let mut effect = DiagnosticNotificationEffect::default();
    effect.refresh_all_diagnostics();
    effect.discard_all_queued_diagnostics = true;
    if push_diagnostics_supported {
        effect.clear_publication_cursor =
            Some(workspace.take_all_diagnostic_publication_uris(None));
    }
    effect
}

fn invalidate_malformed_file_notification(
    workspace: &mut Workspace,
    budget: Option<&ReconciliationBudget>,
    known_endpoints: impl IntoIterator<Item = Url>,
    push_diagnostics_supported: bool,
) -> DiagnosticNotificationEffect {
    let fallback_budget = ReconciliationBudget::new(Arc::new(AtomicBool::new(false)));
    let budget = budget.unwrap_or(&fallback_budget);
    let mut endpoints = Vec::new();
    let mut unique = HashSet::new();
    if endpoints
        .try_reserve(MAX_FILE_OPERATION_RECOVERY_ENDPOINTS)
        .is_err()
        || unique
            .try_reserve(MAX_FILE_OPERATION_RECOVERY_ENDPOINTS)
            .is_err()
    {
        return permanently_fence_file_notification_analysis(
            workspace,
            Some(budget),
            push_diagnostics_supported,
        );
    }
    let mut endpoint_bytes = 0usize;
    for uri in known_endpoints {
        if uri.as_str().len() > MAX_OPEN_DOCUMENT_URI_BYTES {
            return permanently_fence_file_notification_analysis(
                workspace,
                Some(budget),
                push_diagnostics_supported,
            );
        }
        if !unique.insert(uri.clone()) {
            continue;
        }
        let Some(next_bytes) = endpoint_bytes.checked_add(uri.as_str().len()) else {
            return permanently_fence_file_notification_analysis(
                workspace,
                Some(budget),
                push_diagnostics_supported,
            );
        };
        if endpoints.len() >= MAX_FILE_OPERATION_RECOVERY_ENDPOINTS
            || next_bytes > MAX_FILE_OPERATION_RECOVERY_ENDPOINT_BYTES
        {
            return permanently_fence_file_notification_analysis(
                workspace,
                Some(budget),
                push_diagnostics_supported,
            );
        }
        endpoint_bytes = next_bytes;
        endpoints.push(uri);
    }
    invalidate_ambiguous_file_notification(
        workspace,
        Some(budget),
        endpoints,
        push_diagnostics_supported,
    )
}

fn permanently_fence_file_notification_analysis(
    workspace: &mut Workspace,
    budget: Option<&ReconciliationBudget>,
    push_diagnostics_supported: bool,
) -> DiagnosticNotificationEffect {
    // Endpoint evidence was not inspected or cannot fit the explicit
    // notification-recovery envelope. Latch the workspace-wide refusal before
    // emitting any global invalidation effect; never continue with a truncated
    // or unavailable endpoint set.
    let fallback_budget = ReconciliationBudget::new(Arc::new(AtomicBool::new(false)));
    let budget = budget.unwrap_or(&fallback_budget);
    // Latch before recovery is allowed to clear any derived cache. Even if
    // bounded invalidation refuses or is cancelled, this workspace instance
    // cannot serve a result based on unattributable notification state.
    workspace.permanently_fence_notification_analysis(budget);
    workspace.invalidate_for_reconciliation_budget(budget);
    let mut effect = DiagnosticNotificationEffect::default();
    effect.refresh_all_diagnostics();
    effect.discard_all_queued_diagnostics = true;
    if push_diagnostics_supported {
        effect.clear_publication_cursor =
            Some(workspace.take_all_diagnostic_publication_uris(None));
    }
    effect
}

fn mark_publication_root_stale(
    connection: &dyn ProtocolSender,
    workspace: &mut Workspace,
    root_uri: &Url,
) {
    let affected = workspace.mark_diagnostic_publication_root_stale(root_uri);
    connection.discard_diagnostic_publications(&affected);
}

fn mark_refreshed_publication_roots_stale(
    connection: &dyn ProtocolSender,
    workspace: &mut Workspace,
    refresh: &[Url],
    refresh_all: bool,
    pre_stale_targets: Option<&BTreeSet<Url>>,
    budget: &mut DiagnosticPublicationTurnBudget,
    cancel: Option<&AtomicBool>,
) -> Result<(), String> {
    let roots = if refresh_all {
        workspace.open_document_uris()
    } else {
        refresh.to_vec()
    };
    let affected = workspace.mark_diagnostic_publication_roots_stale(&roots, cancel)?;
    budget.stale_root_visits = budget
        .stale_root_visits
        .saturating_add(affected.roots_visited);
    budget.stale_target_visits = budget
        .stale_target_visits
        .saturating_add(affected.target_visits);
    budget.stale_root_uri_bytes = budget
        .stale_root_uri_bytes
        .saturating_add(affected.root_uri_bytes);
    budget.stale_target_uri_bytes = budget
        .stale_target_uri_bytes
        .saturating_add(affected.target_uri_bytes);
    let mut targets = affected.targets;
    if let Some(pre_stale_targets) = pre_stale_targets {
        targets.extend(pre_stale_targets.iter().cloned());
    }
    let mut target_uri_bytes = 0usize;
    let within_target_cap = targets.len() <= MAX_COALESCED_STALE_TARGETS
        && targets.iter().all(|target| {
            target_uri_bytes = target_uri_bytes.saturating_add(target.as_str().len());
            target_uri_bytes <= MAX_COALESCED_STALE_TARGET_URI_BYTES
        });
    if !within_target_cap {
        workspace.invalidate_all_for_file_notification_overflow_bounded();
        let scan = connection.discard_all_diagnostic_publications();
        budget.publication_queue_scans = budget.publication_queue_scans.saturating_add(1);
        budget.publication_queue_messages_scanned = budget
            .publication_queue_messages_scanned
            .saturating_add(scan.scanned_messages);
        budget.publication_queue_bytes_scanned = budget
            .publication_queue_bytes_scanned
            .saturating_add(scan.scanned_bytes);
        return Ok(());
    }
    let scan = if targets.is_empty() {
        DiagnosticPublicationDiscardScan::default()
    } else {
        connection.discard_diagnostic_publications(&targets)
    };
    if !targets.is_empty() {
        budget.publication_queue_scans = budget.publication_queue_scans.saturating_add(1);
    }
    budget.publication_queue_messages_scanned = budget
        .publication_queue_messages_scanned
        .saturating_add(scan.scanned_messages);
    budget.publication_queue_bytes_scanned = budget
        .publication_queue_bytes_scanned
        .saturating_add(scan.scanned_bytes);
    #[cfg(feature = "test-support")]
    if !targets.is_empty() {
        write_publication_staling_test_metrics(
            affected.roots_visited,
            affected.target_visits,
            targets.len(),
            affected.root_uri_bytes,
            target_uri_bytes,
            scan,
        );
    }
    Ok(())
}

#[cfg(feature = "test-support")]
fn write_publication_staling_test_metrics(
    roots_visited: usize,
    target_visits: usize,
    unique_targets: usize,
    root_uri_bytes: usize,
    target_uri_bytes: usize,
    scan: DiagnosticPublicationDiscardScan,
) {
    let Some(path) = std::env::var_os("PASCAL_LSP_TEST_PUBLICATION_STALING_RESULT") else {
        return;
    };
    let metrics = serde_json::json!({
        "roots_visited": roots_visited,
        "target_visits": target_visits,
        "unique_targets": unique_targets,
        "root_uri_bytes": root_uri_bytes,
        "target_uri_bytes": target_uri_bytes,
        "queue_scans": 1,
        "queue_messages_scanned": scan.scanned_messages,
        "queue_bytes_scanned": scan.scanned_bytes,
        "queue_messages_removed": scan.removed_messages,
    });
    let _ = std::fs::write(path, serde_json::to_vec(&metrics).unwrap_or_default());
}

fn handle_notification_with_cancel(
    connection: &dyn ProtocolSender,
    workspace: &mut Workspace,
    notification: Notification,
    workspace_folders_supported: bool,
    push_diagnostics_supported: bool,
    cancel: Option<&AtomicBool>,
    defer_push_clears: bool,
) -> Result<DiagnosticNotificationEffect, String> {
    handle_notification_with_control(
        connection,
        workspace,
        notification,
        workspace_folders_supported,
        push_diagnostics_supported,
        NotificationWorkControl {
            cancel,
            budget: None,
            defer_push_clears,
        },
    )
}

struct NotificationWorkControl<'a> {
    cancel: Option<&'a AtomicBool>,
    budget: Option<&'a ReconciliationBudget>,
    defer_push_clears: bool,
}

fn handle_notification_with_control(
    connection: &dyn ProtocolSender,
    workspace: &mut Workspace,
    notification: Notification,
    workspace_folders_supported: bool,
    push_diagnostics_supported: bool,
    control: NotificationWorkControl<'_>,
) -> Result<DiagnosticNotificationEffect, String> {
    let method = notification.method.clone();
    let budget = control.budget;
    let source_generation = workspace.source_generation();
    let configuration_generation = workspace.configuration_generation();
    let staling = DiagnosticPublicationBatchSender::new(connection, false);
    let result = handle_notification_with_control_inner(
        &staling,
        workspace,
        notification,
        workspace_folders_supported,
        push_diagnostics_supported,
        control,
    );

    // Work-budget exhaustion has a worker-level fallback immediately after
    // this call. Leave that case to the worker so the same reconciliation
    // budget is not applied twice; non-budget partial errors still recover
    // here even when cancellation was not requested.
    let budget_exhausted = budget.is_some_and(ReconciliationBudget::is_exhausted);
    if staling.is_stale_all() && !budget_exhausted {
        return Ok(recover_failed_diagnostic_notification(
            workspace,
            push_diagnostics_supported,
            budget,
        ));
    }

    match result {
        Ok(mut effect) => {
            effect.stale_publication_targets = staling.targets();
            Ok(effect)
        }
        Err(error)
            if !budget_exhausted
                && notification_may_have_mutated_workspace(&method)
                && (staling.staling_requested()
                    || source_generation != workspace.source_generation()
                    || configuration_generation != workspace.configuration_generation()
                    || workspace.analysis_admission_fenced()) =>
        {
            eprintln!("pascal-lsp: notification reconciliation failed closed: {error}");
            Ok(recover_failed_diagnostic_notification(
                workspace,
                push_diagnostics_supported,
                budget,
            ))
        }
        Err(error) => Err(error),
    }
}

fn notification_may_have_mutated_workspace(method: &str) -> bool {
    matches!(
        method,
        "textDocument/didOpen"
            | "textDocument/didChange"
            | "textDocument/didSave"
            | "textDocument/didClose"
            | "workspace/didChangeWatchedFiles"
            | "workspace/didCreateFiles"
            | "workspace/didDeleteFiles"
            | "workspace/didRenameFiles"
            | "workspace/didChangeWorkspaceFolders"
    )
}

fn recover_failed_diagnostic_notification(
    workspace: &mut Workspace,
    push_diagnostics_supported: bool,
    budget: Option<&ReconciliationBudget>,
) -> DiagnosticNotificationEffect {
    if let Some(budget) = budget {
        workspace.invalidate_for_reconciliation_budget(budget);
    } else {
        workspace.invalidate_all_for_file_notification_overflow_bounded();
    }
    let mut effect = DiagnosticNotificationEffect::default();
    effect.refresh_all_diagnostics();
    effect.discard_all_queued_diagnostics = true;
    if push_diagnostics_supported {
        effect.clear_publication_cursor =
            Some(workspace.take_all_diagnostic_publication_uris(None));
    }
    effect
}

fn handle_notification_with_control_inner(
    connection: &dyn ProtocolSender,
    workspace: &mut Workspace,
    notification: Notification,
    workspace_folders_supported: bool,
    push_diagnostics_supported: bool,
    control: NotificationWorkControl<'_>,
) -> Result<DiagnosticNotificationEffect, String> {
    let NotificationWorkControl {
        cancel,
        budget,
        defer_push_clears,
    } = control;
    match notification.method.as_str() {
        "initialized" => Ok(DiagnosticNotificationEffect::default()),
        "textDocument/didOpen" => {
            let params: DidOpenTextDocumentParams = parse_notification(&notification)?;
            let uri = params.text_document.uri.clone();
            if let Err(error) = workspace.open_document(
                params.text_document.uri,
                params.text_document.text,
                params.text_document.version,
            ) {
                eprintln!("pascal-lsp: didOpen ignored: {error}");
                if workspace.analysis_admission_fenced() {
                    let mut effect = DiagnosticNotificationEffect::default();
                    effect.request_refresh();
                    if push_diagnostics_supported {
                        let extra = (uri.as_str().len()
                            <= MAX_DIAGNOSTIC_CLEANUP_URI_BYTES_PER_TARGET)
                            .then(|| uri.clone());
                        effect.cleanup_rejected_uri = extra.clone();
                        effect.clear_publication_cursor =
                            Some(workspace.take_all_diagnostic_publication_uris(extra));
                    }
                    return Ok(effect);
                }
                return Err(error);
            }
            let mut effect = DiagnosticNotificationEffect::default();
            effect.refresh_uri_with_budget(uri.clone(), budget)?;
            effect.refresh_dependents_with_control(workspace, &uri, false, cancel, budget)?;
            // Opening another root can change the ownership set for a shared
            // include even when no source dependency changed.  Recompute roots
            // that have already published a context-sensitive semantic claim;
            // ordinary lint-only roots retain their normal publication order.
            for open_uri in workspace.open_diagnostic_roots_with_semantic_claims() {
                mark_publication_root_stale(connection, workspace, &open_uri);
                effect.refresh_uri(open_uri);
            }
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
                            mark_publication_root_stale(connection, workspace, &uri);
                            let mut effect = DiagnosticNotificationEffect::default();
                            effect.cancel_uri_with_budget(uri.clone(), budget)?;
                            effect.refresh_uri_with_budget(uri.clone(), budget)?;
                            effect.refresh_dependents_with_control(
                                workspace, &uri, false, cancel, budget,
                            )?;
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
            mark_publication_root_stale(connection, workspace, &uri);
            let mut effect = DiagnosticNotificationEffect::default();
            effect.refresh_uri_with_budget(uri.clone(), budget)?;
            effect.refresh_dependents_with_control(workspace, &uri, false, cancel, budget)?;
            for open_uri in workspace.open_document_uris() {
                mark_publication_root_stale(connection, workspace, &open_uri);
                effect.refresh_uri_with_budget(open_uri, budget)?;
            }
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
            mark_publication_root_stale(connection, workspace, &uri);
            let mut effect = DiagnosticNotificationEffect::default();
            effect.refresh_uri_with_budget(uri.clone(), budget)?;
            effect.refresh_dependents_with_control(workspace, &uri, true, cancel, budget)?;
            for open_uri in workspace.open_document_uris() {
                mark_publication_root_stale(connection, workspace, &open_uri);
                effect.refresh_uri_with_budget(open_uri, budget)?;
            }
            Ok(effect)
        }
        "textDocument/didClose" => {
            let params: DidCloseTextDocumentParams = parse_notification(&notification)?;
            let uri = params.text_document.uri;
            let closed = workspace.close_document(&uri);
            if closed {
                mark_publication_root_stale(connection, workspace, &uri);
                let replacement = if push_diagnostics_supported && !defer_push_clears {
                    workspace.stage_clear_diagnostic_publications(&uri)
                } else {
                    workspace.clear_diagnostic_publications(&uri)
                };
                // A rejected-open cleanup cursor already owns the complete
                // retained publication union. Do not bypass its bounded,
                // resumable admission path with synchronous close clears.
                if push_diagnostics_supported
                    && !defer_push_clears
                    && !workspace.analysis_admission_fenced()
                    && replacement.incomplete
                {
                    workspace.mark_pending_diagnostic_publication_incomplete();
                }
            }
            let mut effect = DiagnosticNotificationEffect::default();
            effect.cancel_uri_with_budget(uri.clone(), budget)?;
            if closed {
                effect.request_refresh();
                effect.refresh_dependents_with_control(workspace, &uri, true, cancel, budget)?;
                for open_uri in workspace.open_document_uris() {
                    mark_publication_root_stale(connection, workspace, &open_uri);
                    effect.refresh_uri_with_budget(open_uri, budget)?;
                }
            }
            Ok(effect)
        }
        "workspace/didChangeWatchedFiles" => {
            let Some(changes) = notification.params.get("changes").and_then(Value::as_array) else {
                eprintln!(
                    "pascal-lsp: malformed watched-file batch has no attributable endpoint; fencing workspace analysis"
                );
                return Ok(permanently_fence_file_notification_analysis(
                    workspace,
                    budget,
                    push_diagnostics_supported,
                ));
            };
            if changes.is_empty() {
                return Ok(DiagnosticNotificationEffect::default());
            }
            if changes.len() > MAX_FILE_OPERATION_BATCH_ENTRIES {
                eprintln!(
                    "pascal-lsp: watched-file batch exceeded {MAX_FILE_OPERATION_BATCH_ENTRIES} entries; fencing workspace analysis"
                );
                return Ok(permanently_fence_file_notification_analysis(
                    workspace,
                    budget,
                    push_diagnostics_supported,
                ));
            }
            let mut total_uri_bytes = 0usize;
            let mut parsed_changes = Vec::with_capacity(changes.len());
            let mut oversized_uri_bytes = false;
            let mut malformed_batch = false;
            for change in changes {
                let Some(uri) = change
                    .get("uri")
                    .cloned()
                    .and_then(|value| serde_json::from_value::<Url>(value).ok())
                else {
                    malformed_batch = true;
                    continue;
                };
                let uri = canonical_file_uri(&uri);
                if uri.to_file_path().is_err() {
                    malformed_batch = true;
                    continue;
                }
                if !oversized_uri_bytes
                    && add_file_operation_uri_bytes(&mut total_uri_bytes, &uri).is_err()
                {
                    oversized_uri_bytes = true;
                }
                let kind = match change.get("type").and_then(Value::as_i64) {
                    Some(1) => FileChange::Created,
                    Some(2) => FileChange::Changed,
                    Some(3) => FileChange::Deleted,
                    _ => {
                        malformed_batch = true;
                        continue;
                    }
                };
                parsed_changes.push((uri, kind));
            }
            if malformed_batch {
                eprintln!(
                    "pascal-lsp: malformed watched-file member has unknown batch attribution; fencing workspace analysis"
                );
                return Ok(permanently_fence_file_notification_analysis(
                    workspace,
                    budget,
                    push_diagnostics_supported,
                ));
            }
            if oversized_uri_bytes {
                eprintln!(
                    "pascal-lsp: watched-file batch exceeded {MAX_FILE_OPERATION_BATCH_URI_BYTES} URI bytes; fencing workspace analysis"
                );
                return Ok(permanently_fence_file_notification_analysis(
                    workspace,
                    budget,
                    push_diagnostics_supported,
                ));
            }
            if let Some(budget) = budget {
                for (uri, kind) in &parsed_changes {
                    budget.record_file_event(uri.clone(), *kind);
                }
            }
            let mut effect = DiagnosticNotificationEffect::default();
            for (changed_uri, kind) in parsed_changes {
                if cancel.is_some_and(|cancel| cancel.load(Ordering::Acquire)) {
                    return Err(rename::CANCELLATION_MESSAGE.to_string());
                }
                for uri in workspace.file_event_with_control(&changed_uri, kind, cancel, budget)? {
                    mark_publication_root_stale(connection, workspace, &uri);
                    effect.refresh_uri_with_budget(uri, budget)?;
                }
                if !push_diagnostics_supported {
                    // Pull clients must be told about watched changes even
                    // when the changed source was never opened or indexed by
                    // the legacy push publication map.  Push clients keep
                    // the historical affected-open/dependent set so a
                    // configuration file is not itself analyzed as Pascal.
                    effect.refresh_uri_with_budget(changed_uri.clone(), budget)?;
                }
                effect.refresh_dependents_with_control(
                    workspace,
                    &changed_uri,
                    true,
                    cancel,
                    budget,
                )?;
            }
            Ok(effect)
        }
        "workspace/didCreateFiles" | "workspace/didDeleteFiles" => {
            let created = notification.method == "workspace/didCreateFiles";
            let Some(files) = notification.params.get("files").and_then(Value::as_array) else {
                eprintln!(
                    "pascal-lsp: malformed file-operation batch has unknown endpoint attribution; fencing workspace analysis"
                );
                return Ok(permanently_fence_file_notification_analysis(
                    workspace,
                    budget,
                    push_diagnostics_supported,
                ));
            };
            if files.is_empty() {
                return Ok(invalidate_malformed_file_notification(
                    workspace,
                    budget,
                    [],
                    push_diagnostics_supported,
                ));
            }
            if files.len() > MAX_FILE_OPERATION_BATCH_ENTRIES {
                eprintln!(
                    "pascal-lsp: file-operation batch exceeded {MAX_FILE_OPERATION_BATCH_ENTRIES} entries; fencing workspace analysis"
                );
                return Ok(permanently_fence_file_notification_analysis(
                    workspace,
                    budget,
                    push_diagnostics_supported,
                ));
            }
            let mut uris = Vec::with_capacity(files.len());
            let mut unique = HashSet::with_capacity(files.len());
            let mut total_uri_bytes = 0usize;
            let mut oversized_uri_bytes = false;
            let mut malformed_batch = false;
            for file in files {
                let Some(uri) = file
                    .get("uri")
                    .cloned()
                    .and_then(|value| serde_json::from_value::<Url>(value).ok())
                else {
                    malformed_batch = true;
                    continue;
                };
                let uri = canonical_file_uri(&uri);
                if uri.to_file_path().is_err() {
                    malformed_batch = true;
                    continue;
                }
                if !oversized_uri_bytes
                    && add_file_operation_uri_bytes(&mut total_uri_bytes, &uri).is_err()
                {
                    oversized_uri_bytes = true;
                }
                // Keep the complete canonical endpoint set for accounting and
                // deterministic deduplication before any file-event effects.
                if unique.insert(uri.clone()) {
                    uris.push(uri);
                }
            }
            if malformed_batch {
                eprintln!(
                    "pascal-lsp: malformed file-operation member has unknown batch attribution; fencing workspace analysis"
                );
                return Ok(permanently_fence_file_notification_analysis(
                    workspace,
                    budget,
                    push_diagnostics_supported,
                ));
            }
            if oversized_uri_bytes {
                eprintln!(
                    "pascal-lsp: file-operation batch exceeded {MAX_FILE_OPERATION_BATCH_URI_BYTES} URI bytes; fencing workspace analysis"
                );
                return Ok(permanently_fence_file_notification_analysis(
                    workspace,
                    budget,
                    push_diagnostics_supported,
                ));
            }
            if let Some(budget) = budget {
                for uri in &uris {
                    budget.record_file_event(
                        uri.clone(),
                        if created {
                            FileChange::Created
                        } else {
                            FileChange::Deleted
                        },
                    );
                }
            }
            let mut effect = DiagnosticNotificationEffect::default();
            for uri in uris {
                if cancel.is_some_and(|cancel| cancel.load(Ordering::Acquire)) {
                    return Err(rename::CANCELLATION_MESSAGE.to_string());
                }
                let change = if created {
                    FileChange::Created
                } else {
                    FileChange::Deleted
                };
                for affected in workspace.file_event_with_control(&uri, change, cancel, budget)? {
                    effect.refresh_uri_with_budget(affected, budget)?;
                }
                effect.refresh_uri_with_budget(uri.clone(), budget)?;
                effect.refresh_dependents_with_control(workspace, &uri, true, cancel, budget)?;
            }
            Ok(effect)
        }
        "workspace/didRenameFiles" => {
            let Some(files) = notification.params.get("files").and_then(Value::as_array) else {
                eprintln!(
                    "pascal-lsp: malformed file-rename batch has no attributable endpoint; fencing workspace analysis"
                );
                return Ok(permanently_fence_file_notification_analysis(
                    workspace,
                    budget,
                    push_diagnostics_supported,
                ));
            };
            if files.is_empty() {
                return Ok(permanently_fence_file_notification_analysis(
                    workspace,
                    budget,
                    push_diagnostics_supported,
                ));
            }
            if files.len() > MAX_FILE_OPERATION_BATCH_ENTRIES {
                eprintln!(
                    "pascal-lsp: file-rename batch exceeded {MAX_FILE_OPERATION_BATCH_ENTRIES} entries; fencing workspace analysis"
                );
                return Ok(permanently_fence_file_notification_analysis(
                    workspace,
                    budget,
                    push_diagnostics_supported,
                ));
            }
            let mut renames = Vec::with_capacity(files.len());
            let mut old_uris = HashSet::with_capacity(files.len());
            let mut new_uris = HashSet::with_capacity(files.len());
            let mut unique_renames = HashSet::with_capacity(files.len());
            let mut recoverable_endpoints = Vec::with_capacity(files.len().saturating_mul(2));
            let mut total_uri_bytes = 0usize;
            let mut oversized_uri_bytes = false;
            let mut ambiguous_batch = false;
            let mut malformed_batch = false;
            let mut unretainable_endpoint = false;
            for file in files {
                let old_uri = file
                    .get("oldUri")
                    .cloned()
                    .and_then(|value| serde_json::from_value::<Url>(value).ok());
                let new_uri = file
                    .get("newUri")
                    .cloned()
                    .and_then(|value| serde_json::from_value::<Url>(value).ok());
                let old_uri = old_uri.map(|uri| canonical_file_uri(&uri));
                let new_uri = new_uri.map(|uri| canonical_file_uri(&uri));
                for uri in [old_uri.as_ref(), new_uri.as_ref()].into_iter().flatten() {
                    if uri.to_file_path().is_ok()
                        && uri.as_str().len() > MAX_OPEN_DOCUMENT_URI_BYTES
                    {
                        unretainable_endpoint = true;
                    } else if uri.to_file_path().is_ok() {
                        recoverable_endpoints.push(uri.clone());
                    }
                }
                let (Some(old_uri), Some(new_uri)) = (old_uri, new_uri) else {
                    malformed_batch = true;
                    continue;
                };
                if old_uri.to_file_path().is_err() || new_uri.to_file_path().is_err() {
                    malformed_batch = true;
                    continue;
                }
                if !oversized_uri_bytes
                    && (add_file_operation_uri_bytes(&mut total_uri_bytes, &old_uri).is_err()
                        || add_file_operation_uri_bytes(&mut total_uri_bytes, &new_uri).is_err())
                {
                    oversized_uri_bytes = true;
                }
                if old_uri == new_uri {
                    malformed_batch = true;
                    continue;
                }
                if !unique_renames.insert((old_uri.clone(), new_uri.clone())) {
                    // Replaying the exact same move in one batch is
                    // idempotent; distinct reuse of either endpoint below is
                    // ambiguous and takes the conservative recovery path.
                    continue;
                }
                if !old_uris.insert(old_uri.clone()) || !new_uris.insert(new_uri.clone()) {
                    ambiguous_batch = true;
                }
                renames.push((old_uri, new_uri));
            }
            if unretainable_endpoint {
                eprintln!(
                    "pascal-lsp: file-rename endpoint cannot be retained; fencing workspace analysis"
                );
                return Ok(permanently_fence_file_notification_analysis(
                    workspace,
                    budget,
                    push_diagnostics_supported,
                ));
            }
            if malformed_batch {
                eprintln!(
                    "pascal-lsp: malformed file-rename member has unbounded source identity; fencing workspace analysis"
                );
                return Ok(permanently_fence_file_notification_analysis(
                    workspace,
                    budget,
                    push_diagnostics_supported,
                ));
            }
            if old_uris.iter().any(|uri| new_uris.contains(uri)) {
                ambiguous_batch = true;
            }
            if ambiguous_batch {
                eprintln!(
                    "pascal-lsp: ambiguous file-rename batch; invalidating workspace file state"
                );
                return Ok(invalidate_ambiguous_file_notification(
                    workspace,
                    budget,
                    recoverable_endpoints,
                    push_diagnostics_supported,
                ));
            }
            if oversized_uri_bytes {
                eprintln!(
                    "pascal-lsp: file-rename batch exceeded {MAX_FILE_OPERATION_BATCH_URI_BYTES} URI bytes; fencing workspace analysis"
                );
                return Ok(permanently_fence_file_notification_analysis(
                    workspace,
                    budget,
                    push_diagnostics_supported,
                ));
            }
            if let Some(budget) = budget {
                for (old_uri, new_uri) in &renames {
                    budget.record_rename_endpoint(old_uri.clone());
                    budget.record_rename_endpoint(new_uri.clone());
                    budget.record_file_event(old_uri.clone(), FileChange::Deleted);
                    budget.record_file_event(new_uri.clone(), FileChange::Created);
                }
            }
            let mut effect = DiagnosticNotificationEffect::default();
            for (old_uri, new_uri) in renames {
                if cancel.is_some_and(|cancel| cancel.load(Ordering::Acquire)) {
                    return Err(rename::CANCELLATION_MESSAGE.to_string());
                }
                for affected in
                    workspace.did_rename_file_with_control(&old_uri, &new_uri, cancel, budget)?
                {
                    effect.refresh_uri_with_budget(affected, budget)?;
                }
                effect.refresh_uri_with_budget(old_uri.clone(), budget)?;
                effect.refresh_uri_with_budget(new_uri.clone(), budget)?;
                effect
                    .refresh_dependents_with_control(workspace, &old_uri, true, cancel, budget)?;
                effect
                    .refresh_dependents_with_control(workspace, &new_uri, true, cancel, budget)?;
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
            effect.request_refresh();
            for uri in workspace.open_document_uris() {
                mark_publication_root_stale(connection, workspace, &uri);
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
    connection: &dyn ProtocolSender,
    workspace: &mut Workspace,
    jobs: &mut AnalysisJobs,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    if workspace.analysis_admission_fenced() {
        return Ok(());
    }
    for (uri, version) in
        workspace.take_due_diagnostic_requests_limited(MAX_DIAGNOSTIC_DISPATCHES_PER_TURN)
    {
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
    connection: &dyn ProtocolSender,
    uri: &Url,
    version: Option<i32>,
    diagnostics: Vec<lsp_types::Diagnostic>,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    connection.send_control(diagnostics_notification(uri, version, diagnostics))?;
    Ok(())
}

fn send_diagnostic_clear(
    connection: &dyn ProtocolSender,
    uri: &Url,
    version: Option<i32>,
) -> Result<(), OutputError> {
    connection.send_control(diagnostics_notification(uri, version, Vec::new()))
}

fn diagnostics_notification(
    uri: &Url,
    version: Option<i32>,
    diagnostics: Vec<lsp_types::Diagnostic>,
) -> Message {
    Message::Notification(Notification::new(
        "textDocument/publishDiagnostics".to_string(),
        PublishDiagnosticsParams {
            uri: uri.clone(),
            diagnostics,
            version,
        },
    ))
}

fn diagnostic_publication_uri(message: &Message) -> Option<Url> {
    let Message::Notification(notification) = message else {
        return None;
    };
    if notification.method != "textDocument/publishDiagnostics" {
        return None;
    }
    notification
        .params
        .get("uri")
        .and_then(Value::as_str)
        .and_then(|uri| Url::parse(uri).ok())
}

fn send_diagnostic_publications(
    connection: &dyn ProtocolSender,
    workspace: &mut Workspace,
    root_uri: &Url,
    publications: Vec<queries::DiagnosticPublication>,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    #[cfg(feature = "test-support")]
    let publications = {
        let mut publications = publications;
        if let Some(shared_uri) = std::env::var("PASCAL_LSP_TEST_SHARED_PUBLICATION_TARGET_URI")
            .ok()
            .and_then(|value| Url::parse(&value).ok())
        {
            publications.push(queries::DiagnosticPublication {
                uri: shared_uri,
                version: None,
                diagnostics: vec![lsp_types::Diagnostic::new_simple(
                    lsp_types::Range::default(),
                    "test-only shared publication target".into(),
                )],
            });
        }
        publications
    };
    mark_publication_root_stale(connection, workspace, root_uri);
    let replacement = workspace
        .stage_diagnostic_publications(root_uri, publications)
        .map_err(|error| -> Box<dyn Error + Send + Sync> { error.into() })?;
    if replacement.incomplete {
        workspace.mark_pending_diagnostic_publication_incomplete();
    }
    Ok(())
}

#[cfg(test)]
fn pump_pending_diagnostic_publications(
    connection: &dyn ProtocolSender,
    workspace: &mut Workspace,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    pump_pending_diagnostic_publications_with_budget(
        connection,
        workspace,
        &mut DiagnosticPublicationTurnBudget::default(),
    )
}

fn pump_pending_diagnostic_publications_with_budget(
    connection: &dyn ProtocolSender,
    workspace: &mut Workspace,
    budget: &mut DiagnosticPublicationTurnBudget,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    while budget.processed_targets < MAX_PUSH_DIAGNOSTIC_NOTIFICATIONS_PER_TURN
        && budget.notification_count < MAX_PUSH_DIAGNOSTIC_NOTIFICATIONS_PER_TURN
        && budget.serialized_bytes < MAX_PUSH_DIAGNOSTIC_BYTES_PER_TURN
    {
        let Some((uri, publication, incomplete)) = workspace.peek_pending_diagnostic_publication()
        else {
            break;
        };
        budget.processed_targets += 1;
        if incomplete {
            workspace.mark_pending_diagnostic_publication_incomplete();
        }
        let Some(publication) = publication else {
            workspace.complete_pending_diagnostic_publication(&uri);
            continue;
        };
        let message = diagnostics_notification(
            &publication.uri,
            publication.version,
            publication.diagnostics,
        );
        let bytes = serde_json::to_vec(&message)?
            .len()
            .saturating_add(LSP_FRAME_HEADER_RESERVE_BYTES);
        if bytes > MAX_PUSH_DIAGNOSTIC_NOTIFICATION_BYTES {
            // Omit the complete report rather than publishing a truncated or
            // false-empty notification. The operational warning is retained
            // and retried by a later pump if output is temporarily full.
            workspace.complete_pending_diagnostic_publication(&uri);
            workspace.mark_pending_diagnostic_publication_incomplete();
            continue;
        }
        if budget.serialized_bytes.saturating_add(bytes) > MAX_PUSH_DIAGNOSTIC_BYTES_PER_TURN {
            break;
        }
        match connection.send_control(message) {
            Ok(()) => {
                workspace.complete_pending_diagnostic_publication(&uri);
                budget.notification_count += 1;
                budget.serialized_bytes = budget.serialized_bytes.saturating_add(bytes);
            }
            Err(OutputError::Backpressure) => break,
            Err(OutputError::MessageTooLarge) => {
                workspace.complete_pending_diagnostic_publication(&uri);
                workspace.mark_pending_diagnostic_publication_incomplete();
            }
            Err(error) => return Err(error.into()),
        }
    }
    if budget.notification_count < MAX_PUSH_DIAGNOSTIC_NOTIFICATIONS_PER_TURN
        && workspace.take_pending_diagnostic_publication_incomplete()
    {
        let warning = diagnostic_publication_incomplete_message();
        let bytes = serde_json::to_vec(&warning)?
            .len()
            .saturating_add(LSP_FRAME_HEADER_RESERVE_BYTES);
        if bytes <= MAX_PUSH_DIAGNOSTIC_NOTIFICATION_BYTES
            && budget.serialized_bytes.saturating_add(bytes) <= MAX_PUSH_DIAGNOSTIC_BYTES_PER_TURN
        {
            match connection.send_control(warning) {
                Ok(()) => {
                    budget.notification_count += 1;
                    budget.serialized_bytes = budget.serialized_bytes.saturating_add(bytes);
                }
                Err(OutputError::Backpressure) => {
                    workspace.mark_pending_diagnostic_publication_incomplete();
                }
                Err(error) => return Err(error.into()),
            }
        } else {
            workspace.mark_pending_diagnostic_publication_incomplete();
        }
    }
    Ok(())
}

fn diagnostic_publication_incomplete_message() -> Message {
    Message::Notification(Notification::new(
        "window/showMessage".to_string(),
        ShowMessageParams {
            typ: MessageType::WARNING,
            message: "pascal-lsp: diagnostics are incomplete because the bounded publication limit was reached; no partial related-document report was accepted. This is an operational warning, not a Pascal semantic diagnostic. Retry after reducing workspace diagnostic fan-out.".to_string(),
        },
    ))
}

fn register_file_watcher(
    connection: &dyn ProtocolSender,
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
    connection: &dyn ProtocolSender,
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
    connection: &dyn ProtocolSender,
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
    connection.send_control(Message::Request(request))?;
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
    connection: &dyn ProtocolSender,
    id: RequestId,
    value: T,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let fallback_id = id.clone();
    // `send_result` accepts either the writer queue or the bounded deferred
    // queue.  Only an individually oversized message, or exhaustion of the
    // bounded result budget, becomes a request-scoped error; temporary
    // occupancy of the ordinary control budget is not session-fatal.
    match connection.send_result(Message::Response(Response::new_ok(id, value))) {
        Ok(()) => Ok(()),
        Err(OutputError::MessageTooLarge) => send_error(
            connection,
            fallback_id,
            ErrorCode::RequestFailed,
            "analysis result exceeds the bounded LSP output size; retry with a narrower request",
        ),
        Err(OutputError::ResultBackpressure) => send_error(
            connection,
            fallback_id,
            ErrorCode::RequestFailed,
            "temporary LSP output capacity is full; retry the request after the client drains",
        ),
        Err(error) => Err(error.into()),
    }
}

fn send_error(
    connection: &dyn ProtocolSender,
    id: RequestId,
    code: ErrorCode,
    message: impl Into<String>,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    connection.send_control(Message::Response(Response::new_err(
        id,
        code as i32,
        message.into(),
    )))?;
    Ok(())
}

fn server_capabilities(
    client: &ClientCapabilities,
    workspace_diagnostics_supported: bool,
) -> Value {
    let mut capabilities = serde_json::json!({
        "positionEncoding": "utf-16",
        "textDocumentSync": {
            "openClose": true,
            "change": 2,
            "save": true
        },
        "hoverProvider": {"workDoneProgress": true},
        "completionProvider": {"triggerCharacters": ["."], "resolveProvider": true, "workDoneProgress": true},
        "signatureHelpProvider": {"triggerCharacters": ["(", ","], "workDoneProgress": true},
        "typeDefinitionProvider": {"workDoneProgress": true},
        "declarationProvider": {"workDoneProgress": true},
        "definitionProvider": {"workDoneProgress": true},
        "implementationProvider": {"workDoneProgress": true},
        "documentSymbolProvider": {"workDoneProgress": true},
        "workspaceSymbolProvider": {"workDoneProgress": true},
        "referencesProvider": {"workDoneProgress": true},
        "documentHighlightProvider": {"workDoneProgress": true},
        "selectionRangeProvider": {"workDoneProgress": true},
        "foldingRangeProvider": {"workDoneProgress": true},
        "inlayHintProvider": {"resolveProvider": false, "workDoneProgress": true},
        "callHierarchyProvider": true,
        "typeHierarchyProvider": true,
        "semanticTokensProvider": {
            "legend": crate::NavigationIndex::semantic_tokens_legend(),
            "range": true,
            "full": true,
            "workDoneProgress": true
        },
        "documentFormattingProvider": {"workDoneProgress": true},
        "documentRangeFormattingProvider": {"workDoneProgress": true},
        "documentOnTypeFormattingProvider": {"firstTriggerCharacter": ";"},
        "documentLinkProvider": {"resolveProvider": false},
        "renameProvider": {"prepareProvider": true, "workDoneProgress": true},
        "codeActionProvider": {
            "codeActionKinds": [
                "quickfix",
                "quickfix.implement-interface-method",
                "source.fixAll",
                "source.fixAll.constant-naming",
                "source.fixAll.local-variable-naming",
                "source.organizeImports"
            ],
            "resolveProvider": true,
            "workDoneProgress": true
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
    capabilities["workspace"]["fileOperations"] = serde_json::json!({
        "willCreate": [{"scheme": "file", "pattern": {"glob": "**/*.{pas,pp,pascal}", "matches": "file"}}],
        "willRename": [{"scheme": "file", "pattern": {"glob": "**/*.{pas,pp,pascal}", "matches": "file"}}],
        "willDelete": [{"scheme": "file", "pattern": {"glob": "**/*.{pas,pp,pascal}", "matches": "file"}}],
        "didCreate": [{"scheme": "file", "pattern": {"glob": "**/*.{pas,pp,pascal}", "matches": "file"}}],
        "didRename": [{"scheme": "file", "pattern": {"glob": "**/*.{pas,pp,pascal}", "matches": "file"}}],
        "didDelete": [{"scheme": "file", "pattern": {"glob": "**/*.{pas,pp,pascal}", "matches": "file"}}]
    });
    if supports_pull_diagnostics(client) {
        capabilities["diagnosticProvider"] = serde_json::json!({
            "identifier": SERVER_NAME,
            "interFileDependencies": true,
            "workspaceDiagnostics": workspace_diagnostics_supported,
            "workDoneProgress": true
        });
    }
    capabilities
}

fn client_features(client: &ClientCapabilities) -> ClientFeatures {
    let value = serde_json::to_value(client).unwrap_or(Value::Null);
    let will_rename_files = value
        .pointer("/workspace/fileOperations/willRename")
        .and_then(Value::as_bool)
        .unwrap_or(false);
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
    let rename_file = document_changes
        && value["workspace"]["workspaceEdit"]["resourceOperations"]
            .as_array()
            .is_some_and(|operations| {
                operations
                    .iter()
                    .any(|operation| operation.as_str() == Some("rename"))
            });
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
    let completion_snippet_support =
        value["textDocument"]["completion"]["completionItem"]["snippetSupport"]
            .as_bool()
            .unwrap_or(false);
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
        rename_file,
        will_rename_files,
        hierarchical_document_symbols,
        hover_format,
        completion_format,
        completion_snippet_support,
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

fn supports_pull_diagnostics(client: &ClientCapabilities) -> bool {
    client
        .text_document
        .as_ref()
        .and_then(|text_document| text_document.diagnostic.as_ref())
        .is_some()
}

fn supports_related_diagnostics(client: &ClientCapabilities) -> bool {
    client
        .text_document
        .as_ref()
        .and_then(|text_document| text_document.diagnostic.as_ref())
        .and_then(|diagnostic| diagnostic.related_document_support)
        .unwrap_or(false)
}

fn supports_diagnostic_refresh(client: &ClientCapabilities, raw_initialize: &Value) -> bool {
    client
        .workspace
        .as_ref()
        .and_then(|workspace| workspace.diagnostic.as_ref())
        .and_then(|diagnostic| diagnostic.refresh_support)
        .unwrap_or_else(|| {
            raw_initialize
                .pointer("/capabilities/workspace/diagnostics/refreshSupport")
                .and_then(Value::as_bool)
                .unwrap_or(false)
        })
}

fn is_affected_neovim_document_pull_client(raw_initialize: &Value) -> bool {
    raw_initialize
        .pointer("/clientInfo/name")
        .and_then(Value::as_str)
        .is_some_and(|name| name == "Neovim")
        && raw_initialize
            .pointer("/clientInfo/version")
            .and_then(Value::as_str)
            .is_some_and(|version| {
                matches!(
                    version.trim(),
                    // Neovim 0.12.5 reports the first form from its LSP
                    // clientInfo and the second form is accepted for clients
                    // that include the usual display-version prefix.
                    "0.12.5" | "v0.12.5" | "0.12.5+v0.12.5" | "v0.12.5+v0.12.5"
                )
            })
}

fn supports_workspace_diagnostic_reports(raw_initialize: &Value) -> bool {
    // `workspace.diagnostics` is the canonical LSP 3.17 capability spelling.
    // The pinned lsp-types version still exposes the older singular field, so
    // inspect the raw initialize value only to accept either workspace wire
    // spelling.  Refresh support is optional and does not gate reports.
    //
    // The evidenced Neovim 0.12.5 client advertises the canonical capability
    // but deliberately keeps attached buffers in document-pull mode.  Its
    // workspace refresh handler then ignores workspace reports for those
    // buffers and opens every unopened URI returned by the scan.  Keep only
    // that exact, versioned client on document pull; versionless, unknown, and
    // newer clients still receive the standard workspace provider.
    let neovim_document_pull_compat = is_affected_neovim_document_pull_client(raw_initialize);
    !neovim_document_pull_compat
        && (raw_initialize
            .pointer("/capabilities/workspace/diagnostics")
            .is_some_and(Value::is_object)
            || raw_initialize
                .pointer("/capabilities/workspace/diagnostic")
                .is_some_and(Value::is_object))
}

fn supports_configuration(client: &ClientCapabilities) -> bool {
    let supported = client
        .workspace
        .as_ref()
        .and_then(|workspace| workspace.configuration)
        .unwrap_or(false);
    supported
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
        AnalysisJobId, AnalysisJobs, AnalysisPriority, AnalysisProgressTokens, AnalysisRequest,
        AnalysisResult, AnalysisResultValue, BoundedReader, ClientFeatures, CompletionAnalysis,
        CompletionResolutionSeed, CompletionResolutionStore, CompletionResult,
        ConfigurationCoordinator, DiagnosticPublicationDiscardScan,
        DiagnosticPublicationTurnBudget, DiagnosticPullStore, DocumentationFormat,
        FileWatcherRegistration, MAX_ANALYSIS_QUEUE, MAX_CLIENT_ANALYSIS_RECIPIENTS,
        MAX_COMPLETION_RESOLUTION_CONTEXT_BYTES, MAX_COMPLETION_RESOLUTION_DATA_BYTES,
        MAX_COMPLETION_RESOLUTION_RECORDS, MAX_CONFIGURATION_WATCH_PATHS,
        MAX_PARTIAL_RESULT_BYTES_PER_CHUNK, MAX_PAYLOAD_BYTES, MAX_PENDING_OUTBOUND_CONTROL_BYTES,
        MAX_PENDING_OUTBOUND_CONTROL_MESSAGES, MAX_PENDING_OUTBOUND_DATA_MESSAGES,
        MAX_WATCHER_REGISTRATION_RETRIES, OutboundClass, OutboundQueue, OutputError,
        PartialDelivery, PartialDeliveryRecipient, PartialDeliveryValidation, PartialResultPayload,
        PendingAnalysis, PriorityQueue, ProtocolSender, TestBarrierConfig, deliver_analysis_result,
        event_loop_receive_timeout, invalidate_analysis_result,
        pump_pending_diagnostic_publications, supports_diagnostic_refresh,
        supports_workspace_diagnostic_reports,
    };
    use crate::workspace::queries::DiagnosticPublication;
    use crate::workspace::rename::{SourceRecord, install_snapshot_priority_barrier};
    use crate::workspace::{Workspace, WorkspaceOptions};
    use crossbeam_channel::{RecvTimeoutError, bounded};
    use lsp_server::{Connection, Message, Notification, RequestId, Response};
    use lsp_types::{
        ClientCapabilities, CompletionItem, CompletionList, Diagnostic, DiagnosticSeverity,
        Location, MarkupKind, Position, PrepareRenameResponse, Range, SymbolInformation,
        SymbolKind, Url,
    };
    use pascal_project::delphi_overrides::OverrideSession;
    use std::cell::RefCell;
    use std::fs;
    use std::io::{Cursor, ErrorKind};
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::atomic::AtomicBool;
    use std::sync::mpsc;
    use std::thread;
    use std::time::{Duration, Instant};

    struct BackpressureOnceSender {
        should_block: AtomicBool,
        accepted: Mutex<Vec<Message>>,
    }

    #[derive(Default)]
    struct RecordingDiscardSender {
        discarded: Mutex<Vec<std::collections::BTreeSet<Url>>>,
    }

    struct QueueDiscardSender {
        sender: crossbeam_channel::Sender<Message>,
        outbound: RefCell<super::OutboundQueue>,
        scans: Mutex<Vec<DiagnosticPublicationDiscardScan>>,
    }

    impl QueueDiscardSender {
        fn flush(&self) -> Result<(), OutputError> {
            self.outbound.borrow_mut().flush(&self.sender)
        }
    }

    impl ProtocolSender for QueueDiscardSender {
        fn send_control(&self, message: Message) -> Result<(), OutputError> {
            self.outbound.borrow_mut().enqueue(
                &self.sender,
                message,
                super::OutboundClass::Control,
            )?;
            Ok(())
        }

        fn send_result(&self, message: Message) -> Result<(), OutputError> {
            self.outbound.borrow_mut().enqueue(
                &self.sender,
                message,
                super::OutboundClass::Result,
            )?;
            Ok(())
        }

        fn send_data(&self, message: Message) -> Result<bool, OutputError> {
            self.outbound
                .borrow_mut()
                .enqueue(&self.sender, message, super::OutboundClass::Data)
        }

        fn discard_diagnostic_publications(
            &self,
            uris: &std::collections::BTreeSet<Url>,
        ) -> DiagnosticPublicationDiscardScan {
            let scan = self
                .outbound
                .borrow_mut()
                .discard_diagnostic_publications(uris);
            self.scans.lock().expect("queue scan log").push(scan);
            scan
        }

        fn discard_all_diagnostic_publications(&self) -> DiagnosticPublicationDiscardScan {
            let scan = self
                .outbound
                .borrow_mut()
                .discard_all_diagnostic_publications();
            self.scans.lock().expect("queue scan log").push(scan);
            scan
        }
    }

    impl ProtocolSender for RecordingDiscardSender {
        fn send_control(&self, _message: Message) -> Result<(), OutputError> {
            Ok(())
        }

        fn send_result(&self, _message: Message) -> Result<(), OutputError> {
            Ok(())
        }

        fn send_data(&self, _message: Message) -> Result<bool, OutputError> {
            Ok(true)
        }

        fn discard_diagnostic_publications(
            &self,
            uris: &std::collections::BTreeSet<Url>,
        ) -> DiagnosticPublicationDiscardScan {
            self.discarded
                .lock()
                .expect("discard calls")
                .push(uris.clone());
            DiagnosticPublicationDiscardScan::default()
        }
    }

    impl ProtocolSender for BackpressureOnceSender {
        fn send_control(&self, message: Message) -> Result<(), OutputError> {
            if self
                .should_block
                .swap(false, std::sync::atomic::Ordering::AcqRel)
            {
                return Err(OutputError::Backpressure);
            }
            self.accepted.lock().expect("sender lock").push(message);
            Ok(())
        }

        fn send_result(&self, message: Message) -> Result<(), OutputError> {
            self.send_control(message)
        }

        fn send_data(&self, message: Message) -> Result<bool, OutputError> {
            self.send_control(message).map(|()| true)
        }
    }

    fn test_workspace(
        roots: Vec<PathBuf>,
        options: crate::workspace::WorkspaceOptions,
    ) -> Workspace {
        Workspace::with_override_session(roots, options, OverrideSession::new(None))
    }

    #[test]
    fn refreshed_publication_roots_coalesce_shared_targets_before_discard() {
        let temp = tempfile::tempdir().expect("workspace");
        let mut workspace =
            test_workspace(vec![temp.path().to_path_buf()], WorkspaceOptions::default());
        let roots = (0..8)
            .map(|index| {
                let path = temp.path().join(format!("Root{index}.pas"));
                fs::write(
                    &path,
                    format!("unit Root{index}; interface implementation end."),
                )
                .expect("root source");
                Url::from_file_path(path).expect("root URI")
            })
            .collect::<Vec<_>>();
        let shared_target =
            Url::from_file_path(temp.path().join("Shared.inc")).expect("shared target");
        let private_target =
            Url::from_file_path(temp.path().join("Private.inc")).expect("private target");
        for (index, root) in roots.iter().enumerate() {
            workspace
                .open_document(
                    root.clone(),
                    format!("unit Root{index}; interface implementation end."),
                    1,
                )
                .expect("open diagnostic root");
            let mut publications = vec![DiagnosticPublication {
                uri: shared_target.clone(),
                version: None,
                diagnostics: vec![Diagnostic::new_simple(Range::default(), "shared".into())],
            }];
            if index == 0 {
                publications.push(DiagnosticPublication {
                    uri: private_target.clone(),
                    version: None,
                    diagnostics: vec![Diagnostic::new_simple(Range::default(), "private".into())],
                });
            }
            workspace
                .stage_diagnostic_publications(root, publications)
                .expect("retain root publications");
        }
        let protocol = RecordingDiscardSender::default();

        super::mark_refreshed_publication_roots_stale(
            &protocol,
            &mut workspace,
            &roots,
            false,
            None,
            &mut DiagnosticPublicationTurnBudget::default(),
            None,
        )
        .expect("coalesce refreshed roots");

        let discarded = protocol.discarded.lock().expect("discard calls");
        assert_eq!(
            discarded.len(),
            1,
            "all refreshed roots must share one outbound queue scan"
        );
        assert_eq!(
            discarded[0],
            std::collections::BTreeSet::from([shared_target, private_target])
        );
    }

    #[test]
    fn did_change_coalesces_stale_roots_with_no_publication_targets() {
        const ROOTS: usize = 96;
        let temp = tempfile::tempdir().expect("workspace");
        let mut workspace =
            test_workspace(vec![temp.path().to_path_buf()], WorkspaceOptions::default());
        let roots = (0..ROOTS)
            .map(|index| {
                let path = temp.path().join(format!("Open{index:03}.pas"));
                let text = format!("unit Open{index:03}; interface implementation end.");
                fs::write(&path, &text).expect("write root");
                (Url::from_file_path(path).expect("root URI"), text)
            })
            .collect::<Vec<_>>();
        for (uri, text) in &roots {
            workspace
                .open_document(uri.clone(), text.clone(), 1)
                .expect("open root");
        }
        let (changed_root, changed_text) = &roots[0];
        let target = Url::from_file_path(temp.path().join("Related.inc")).expect("target URI");
        workspace
            .stage_diagnostic_publications(
                changed_root,
                [DiagnosticPublication {
                    uri: target.clone(),
                    version: None,
                    diagnostics: vec![Diagnostic::new_simple(Range::default(), "old".into())],
                }],
            )
            .expect("retain publication");
        let unrelated =
            Url::from_file_path(temp.path().join("Unrelated.inc")).expect("unrelated target URI");
        let (transport, receiver) = bounded(1);
        transport
            .send(Message::Notification(Notification::new(
                "$/occupied".to_string(),
                serde_json::Value::Null,
            )))
            .expect("occupy slow writer");
        let protocol = QueueDiscardSender {
            sender: transport,
            outbound: RefCell::new(super::OutboundQueue::default()),
            scans: Mutex::new(Vec::new()),
        };
        for uri in [&target, &unrelated] {
            protocol
                .send_control(super::diagnostics_notification(
                    uri,
                    None,
                    vec![Diagnostic::new_simple(Range::default(), "queued".into())],
                ))
                .expect("queue diagnostic behind writer");
        }
        let effect = super::handle_notification_with_cancel(
            &protocol,
            &mut workspace,
            Notification::new(
                "textDocument/didChange".to_string(),
                serde_json::json!({
                    "textDocument": {"uri": changed_root, "version": 2},
                    "contentChanges": [{"text": changed_text}],
                }),
            ),
            false,
            true,
            None,
            false,
        )
        .expect("valid didChange");
        let mut budget = DiagnosticPublicationTurnBudget::default();
        super::mark_refreshed_publication_roots_stale(
            &protocol,
            &mut workspace,
            &effect.refresh,
            effect.refresh_all_diagnostics,
            Some(&effect.stale_publication_targets),
            &mut budget,
            None,
        )
        .expect("apply event-loop stale batch");

        let scans = protocol.scans.lock().expect("queue scan log");
        assert_eq!(scans.len(), 1, "one notification must cause one queue scan");
        assert_eq!(scans[0].scanned_messages, 2);
        assert_eq!(scans[0].removed_messages, 1);
        assert!(scans[0].scanned_bytes > 0);
        assert_eq!(budget.publication_queue_scans, 1);
        assert_eq!(budget.publication_queue_messages_scanned, 2);
        assert!(budget.publication_queue_bytes_scanned > 0);
        drop(scans);

        protocol
            .send_result(Message::Response(Response::new_ok(
                RequestId::from("unrelated-request".to_string()),
                serde_json::json!({"ok": true}),
            )))
            .expect("queue unrelated request response");
        receiver.try_recv().expect("release slow writer");
        let mut delivered = Vec::new();
        loop {
            protocol.flush().expect("resume writer");
            while let Ok(message) = receiver.try_recv() {
                delivered.push(message);
            }
            if !protocol.outbound.borrow().has_pending() {
                break;
            }
        }
        let published_uris = delivered
            .iter()
            .filter_map(|message| match message {
                Message::Notification(notification)
                    if notification.method == "textDocument/publishDiagnostics" =>
                {
                    notification.params["uri"].as_str()
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(!published_uris.contains(&target.as_str()));
        assert!(published_uris.contains(&unrelated.as_str()));
        assert!(delivered.iter().any(|message| matches!(
            message,
            Message::Response(response) if response.id == RequestId::from("unrelated-request".to_string())
        )));
    }

    #[test]
    fn stale_root_batch_with_no_retained_targets_skips_queue_filter() {
        let temp = tempfile::tempdir().expect("workspace");
        let mut workspace =
            test_workspace(vec![temp.path().to_path_buf()], WorkspaceOptions::default());
        let roots = (0..8)
            .map(|index| {
                let path = temp.path().join(format!("Unpublished{index}.pas"));
                let text = format!("unit Unpublished{index}; interface implementation end.");
                fs::write(&path, &text).expect("write root");
                let uri = Url::from_file_path(path).expect("root URI");
                workspace
                    .open_document(uri.clone(), text, 1)
                    .expect("open root");
                uri
            })
            .collect::<Vec<_>>();
        let protocol = RecordingDiscardSender::default();

        super::mark_refreshed_publication_roots_stale(
            &protocol,
            &mut workspace,
            &roots,
            false,
            None,
            &mut DiagnosticPublicationTurnBudget::default(),
            None,
        )
        .expect("mark roots without retained reports");

        assert!(protocol.discarded.lock().expect("discard calls").is_empty());
    }

    #[test]
    fn cancelled_did_change_returns_fail_closed_cleanup_effect() {
        let temp = tempfile::tempdir().expect("workspace");
        let mut workspace =
            test_workspace(vec![temp.path().to_path_buf()], WorkspaceOptions::default());
        let root_path = temp.path().join("Cancelled.pas");
        let old_text = "unit Cancelled; interface implementation end.";
        fs::write(&root_path, old_text).expect("write root");
        let root = Url::from_file_path(root_path).expect("root URI");
        workspace
            .open_document(root.clone(), old_text.to_string(), 1)
            .expect("open root");
        let target = Url::from_file_path(temp.path().join("Cancelled.inc")).expect("target URI");
        let (transport, receiver) = bounded(1);
        transport
            .send(Message::Notification(Notification::new(
                "$/occupied".to_string(),
                serde_json::Value::Null,
            )))
            .expect("occupy writer");
        let protocol = QueueDiscardSender {
            sender: transport,
            outbound: RefCell::new(super::OutboundQueue::default()),
            scans: Mutex::new(Vec::new()),
        };
        protocol
            .send_control(super::diagnostics_notification(
                &target,
                None,
                vec![Diagnostic::new_simple(Range::default(), "stale".into())],
            ))
            .expect("queue stale report");
        let cancelled = AtomicBool::new(true);
        let budget = super::ReconciliationBudget::new(Arc::new(AtomicBool::new(false)));

        let effect = super::handle_notification_with_control(
            &protocol,
            &mut workspace,
            Notification::new(
                "textDocument/didChange".to_string(),
                serde_json::json!({
                    "textDocument": {"uri": root, "version": 2},
                    "contentChanges": [{"text": "unit Cancelled; interface implementation end."}],
                }),
            ),
            false,
            true,
            super::NotificationWorkControl {
                cancel: Some(&cancelled),
                budget: Some(&budget),
                defer_push_clears: false,
            },
        )
        .expect("partial mutation becomes fail-closed effect");

        assert!(effect.refresh_all_diagnostics);
        assert!(effect.discard_all_queued_diagnostics);
        assert!(effect.clear_publication_cursor.is_some());
        let scan = protocol.discard_all_diagnostic_publications();
        assert_eq!(scan.removed_messages, 1);
        assert_eq!(protocol.scans.lock().expect("scan log").len(), 1);
        receiver.try_recv().expect("release writer");
        protocol.flush().expect("flush after fail-closed discard");
        assert!(receiver.try_recv().is_err(), "stale report must not escape");
    }

    #[test]
    fn completed_diagnostic_jobs_coalesce_staleness_for_one_poll_turn() {
        const JOBS: usize = 3;
        let temp = tempfile::tempdir().expect("workspace");
        let mut workspace =
            test_workspace(vec![temp.path().to_path_buf()], WorkspaceOptions::default());
        let target = Url::from_file_path(temp.path().join("Shared.inc")).expect("target URI");
        let roots = (0..JOBS)
            .map(|index| {
                let path = temp.path().join(format!("Job{index}.pas"));
                let text = format!("unit Job{index}; interface implementation end.");
                fs::write(&path, &text).expect("write root");
                let uri = Url::from_file_path(path).expect("root URI");
                workspace
                    .open_document(uri.clone(), text, 1)
                    .expect("open root");
                workspace
                    .stage_diagnostic_publications(
                        &uri,
                        [DiagnosticPublication {
                            uri: target.clone(),
                            version: None,
                            diagnostics: vec![Diagnostic::new_simple(
                                Range::default(),
                                "old".into(),
                            )],
                        }],
                    )
                    .expect("retain old publication");
                uri
            })
            .collect::<Vec<_>>();
        let (transport, receiver) = bounded(1);
        transport
            .send(Message::Notification(Notification::new(
                "$/occupied".to_string(),
                serde_json::Value::Null,
            )))
            .expect("occupy writer channel");
        let protocol = QueueDiscardSender {
            sender: transport,
            outbound: RefCell::new(super::OutboundQueue::default()),
            scans: Mutex::new(Vec::new()),
        };
        for _ in 0..JOBS {
            protocol
                .send_control(super::diagnostics_notification(
                    &target,
                    None,
                    vec![Diagnostic::new_simple(
                        Range::default(),
                        "old queued".into(),
                    )],
                ))
                .expect("queue old diagnostic behind slow writer");
        }

        let mut jobs = AnalysisJobs::new();
        for (index, uri) in roots.into_iter().enumerate() {
            let id = super::AnalysisComputationId(20_000 + index as u64);
            let cancellation = Arc::new(AtomicBool::new(false));
            jobs.diagnostics.insert(
                id,
                super::PendingDiagnostic {
                    uri: uri.clone(),
                    analysis: super::PendingAnalysis {
                        cancellation,
                        handle: thread::spawn(|| {}),
                        recipients: Vec::new(),
                        key: None,
                    },
                },
            );
            jobs.diagnostic_jobs.insert(uri.clone(), id);
            jobs.sender
                .send(super::AnalysisResult {
                    id: super::AnalysisJobId::Diagnostic(id),
                    source_generation: workspace.source_generation(),
                    configuration_generation: workspace.configuration_generation(),
                    records: Vec::new(),
                    value: super::AnalysisResultValue::Diagnostics(super::DiagnosticsAnalysis {
                        uri,
                        version: Some(1),
                        value: Ok(vec![DiagnosticPublication {
                            uri: target.clone(),
                            version: None,
                            diagnostics: vec![Diagnostic::new_simple(
                                Range::default(),
                                format!("fresh-{index}"),
                            )],
                        }]),
                        discard: false,
                    }),
                })
                .expect("queue completed diagnostic job");
        }
        let mut budget = DiagnosticPublicationTurnBudget::default();
        jobs.poll_with_diagnostic_budget(&protocol, &mut workspace, &mut budget, true)
            .expect("poll completed diagnostics");

        let scans = protocol.scans.lock().expect("queue scan log");
        assert_eq!(scans.len(), 1);
        assert_eq!(scans[0].scanned_messages, JOBS);
        assert_eq!(scans[0].removed_messages, JOBS);
        assert!(scans[0].scanned_bytes > 0);
        assert_eq!(budget.publication_queue_scans, 1);
        assert_eq!(budget.publication_queue_messages_scanned, JOBS);
        assert!(budget.publication_queue_bytes_scanned > 0);
        drop(scans);

        receiver.try_recv().expect("release slow writer slot");
        let mut delivered = Vec::new();
        loop {
            protocol.flush().expect("resume writer");
            while let Ok(message) = receiver.try_recv() {
                delivered.push(message);
            }
            if !protocol.outbound.borrow().has_pending() {
                break;
            }
        }
        let diagnostic_messages = delivered
            .into_iter()
            .filter_map(|message| match message {
                Message::Notification(notification)
                    if notification.method == "textDocument/publishDiagnostics" =>
                {
                    Some(notification.params)
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(diagnostic_messages.len(), 1);
        let fresh_messages = diagnostic_messages[0]["diagnostics"]
            .as_array()
            .expect("aggregate fresh reports");
        assert_eq!(fresh_messages.len(), JOBS);
        assert!(fresh_messages.iter().all(|diagnostic| {
            diagnostic["message"]
                .as_str()
                .is_some_and(|message| message.starts_with("fresh-"))
        }));
    }

    #[test]
    fn publication_limit_notice_is_operational_not_a_semantic_diagnostic() {
        let Message::Notification(notification) =
            super::diagnostic_publication_incomplete_message()
        else {
            panic!("publication limit notice must be a notification");
        };
        assert_eq!(notification.method, "window/showMessage");
        let params = notification.params;
        assert_eq!(params["type"], 2);
        let message = params["message"].as_str().expect("warning message");
        assert!(message.contains("diagnostics are incomplete"));
        assert!(message.contains("not a Pascal semantic diagnostic"));
        assert!(!message.contains("publishDiagnostics"));
    }

    #[test]
    fn push_publication_overflow_sends_only_a_client_visible_incomplete_notice() {
        let temp = tempfile::tempdir().expect("workspace");
        let mut workspace = test_workspace(
            vec![temp.path().to_path_buf()],
            crate::workspace::WorkspaceOptions::default(),
        );
        workspace.seed_diagnostic_publication_capacity_for_test();
        let root = Url::from_file_path(temp.path().join("new-owner.pas")).expect("root URI");
        let omitted =
            Url::from_file_path(temp.path().join("nonempty-related.pas")).expect("related URI");
        let (server, client) = Connection::memory();

        super::send_diagnostic_publications(
            &server,
            &mut workspace,
            &root,
            vec![DiagnosticPublication {
                uri: omitted.clone(),
                version: None,
                diagnostics: vec![lsp_types::Diagnostic::new_simple(
                    lsp_types::Range::default(),
                    "real semantic finding".to_string(),
                )],
            }],
        )
        .expect("over-cap report should be downgraded to an operational warning");
        pump_pending_diagnostic_publications(&server, &mut workspace)
            .expect("deliver incomplete notice");

        let Message::Notification(notification) = client
            .receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("client-visible incomplete notice")
        else {
            panic!("incomplete notice should be a notification");
        };
        assert_eq!(notification.method, "window/showMessage");
        assert!(
            notification.params["message"]
                .as_str()
                .is_some_and(|message| message.contains("diagnostics are incomplete"))
        );
        assert!(client.receiver.try_recv().is_err());
    }

    #[test]
    fn push_publication_after_another_root_overflows_excludes_stale_findings() {
        let temp = tempfile::tempdir().expect("workspace");
        let mut workspace = test_workspace(
            vec![temp.path().to_path_buf()],
            crate::workspace::WorkspaceOptions::default(),
        );
        let root_a = Url::from_file_path(temp.path().join("A.pas")).expect("root A URI");
        let root_b = Url::from_file_path(temp.path().join("B.pas")).expect("root B URI");
        let target = Url::from_file_path(temp.path().join("Shared.inc")).expect("target URI");
        let (server, client) = Connection::memory();

        super::send_diagnostic_publications(
            &server,
            &mut workspace,
            &root_a,
            vec![DiagnosticPublication {
                uri: target.clone(),
                version: None,
                diagnostics: vec![lsp_types::Diagnostic::new_simple(
                    lsp_types::Range::default(),
                    "A stale finding".into(),
                )],
            }],
        )
        .expect("initial A publication");
        pump_pending_diagnostic_publications(&server, &mut workspace)
            .expect("deliver initial A publication");
        let Message::Notification(initial) = client
            .receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("initial publishDiagnostics")
        else {
            panic!("initial message should be a notification");
        };
        assert_eq!(initial.method, "textDocument/publishDiagnostics");
        assert!(initial.params["diagnostics"][0]["message"] == "A stale finding");

        let too_long = Url::parse(&format!(
            "file:///{}",
            "x".repeat(crate::workspace::MAX_PUBLISHED_DIAGNOSTIC_URI_BYTES)
        ))
        .expect("over-limit URI");
        super::send_diagnostic_publications(
            &server,
            &mut workspace,
            &root_a,
            vec![DiagnosticPublication {
                uri: too_long,
                version: None,
                diagnostics: vec![lsp_types::Diagnostic::new_simple(
                    lsp_types::Range::default(),
                    "A no longer has this finding".into(),
                )],
            }],
        )
        .expect("overflow should be reported without partial publications");
        pump_pending_diagnostic_publications(&server, &mut workspace)
            .expect("deliver overflow warning");
        let Message::Notification(overflow_notice) = client
            .receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("overflow warning")
        else {
            panic!("overflow should send a warning notification");
        };
        assert_eq!(overflow_notice.method, "window/showMessage");

        super::send_diagnostic_publications(
            &server,
            &mut workspace,
            &root_b,
            vec![DiagnosticPublication {
                uri: target.clone(),
                version: None,
                diagnostics: vec![lsp_types::Diagnostic::new_simple(
                    lsp_types::Range::default(),
                    "B current finding".into(),
                )],
            }],
        )
        .expect("B publication must omit A's stale contribution");
        pump_pending_diagnostic_publications(&server, &mut workspace)
            .expect("deliver B's current report and incomplete warning");
        let Message::Notification(update) = client
            .receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("B's current publishDiagnostics")
        else {
            panic!("B's report should be a notification");
        };
        assert_eq!(update.method, "textDocument/publishDiagnostics");
        assert_eq!(update.params["uri"], target.as_str());
        assert_eq!(update.params["diagnostics"].as_array().unwrap().len(), 1);
        assert_eq!(
            update.params["diagnostics"][0]["message"],
            "B current finding"
        );
        assert!(
            update.params["diagnostics"]
                .as_array()
                .unwrap()
                .iter()
                .all(|diagnostic| diagnostic["message"] != "A stale finding")
        );
        let Message::Notification(incomplete_notice) = client
            .receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("aggregate incompleteness warning")
        else {
            panic!("incomplete aggregate should be a warning notification");
        };
        assert_eq!(incomplete_notice.method, "window/showMessage");
        assert!(client.receiver.try_recv().is_err());
    }

    #[test]
    fn staged_publications_are_bounded_ordered_and_revalidated_after_supersession() {
        const TARGETS: usize = 70;
        let temp = tempfile::tempdir().expect("workspace");
        let mut workspace = test_workspace(
            vec![temp.path().to_path_buf()],
            crate::workspace::WorkspaceOptions::default(),
        );
        let root = Url::from_file_path(temp.path().join("A.pas")).expect("root URI");
        let base = temp.path().to_path_buf();
        let publications = |message: &'static str| {
            (0..TARGETS)
                .map(|index| DiagnosticPublication {
                    uri: Url::from_file_path(base.join(format!("Related{index:02}.inc")))
                        .expect("related URI"),
                    version: None,
                    diagnostics: vec![Diagnostic::new_simple(Range::default(), message.into())],
                })
                .collect::<Vec<_>>()
        };
        let (server, client) = Connection::memory();
        super::send_diagnostic_publications(&server, &mut workspace, &root, publications("old"))
            .expect("stage first generation");
        super::send_diagnostic_publications(
            &server,
            &mut workspace,
            &root,
            publications("current"),
        )
        .expect("supersede before output admission");

        pump_pending_diagnostic_publications(&server, &mut workspace)
            .expect("emit first bounded batch");
        let first_batch = (0..64)
            .map(|_| {
                client
                    .receiver
                    .recv_timeout(Duration::from_secs(1))
                    .expect("first turn notification")
            })
            .collect::<Vec<_>>();
        assert!(first_batch.iter().all(|message| {
            matches!(message, Message::Notification(notification)
                if notification.params["diagnostics"][0]["message"] == "current")
        }));
        assert_eq!(workspace.pending_diagnostic_publication_count(), 6);
        assert!(client.receiver.try_recv().is_err());

        pump_pending_diagnostic_publications(&server, &mut workspace)
            .expect("resume remaining targets");
        let last_batch = (0..6)
            .map(|_| {
                client
                    .receiver
                    .recv_timeout(Duration::from_secs(1))
                    .expect("resumed turn notification")
            })
            .collect::<Vec<_>>();
        assert!(last_batch.iter().all(|message| {
            matches!(message, Message::Notification(notification)
                if notification.params["diagnostics"][0]["message"] == "current")
        }));
        assert_eq!(workspace.pending_diagnostic_publication_count(), 0);
        assert!(client.receiver.try_recv().is_err());
    }

    #[test]
    fn source_version_supersession_marks_queued_owner_stale_before_aggregate() {
        let temp = tempfile::tempdir().expect("workspace");
        let mut workspace = test_workspace(
            vec![temp.path().to_path_buf()],
            crate::workspace::WorkspaceOptions::default(),
        );
        let root_a = Url::from_file_path(temp.path().join("A.pas")).expect("root A URI");
        let root_b = Url::from_file_path(temp.path().join("B.pas")).expect("root B URI");
        let target = Url::from_file_path(temp.path().join("Shared.inc")).expect("target URI");
        let (server, client) = Connection::memory();
        for (root, message) in [(&root_a, "A old version"), (&root_b, "B current")] {
            super::send_diagnostic_publications(
                &server,
                &mut workspace,
                root,
                vec![DiagnosticPublication {
                    uri: target.clone(),
                    version: None,
                    diagnostics: vec![Diagnostic::new_simple(Range::default(), message.into())],
                }],
            )
            .expect("stage owner snapshot");
        }
        workspace.mark_diagnostic_publication_root_stale(&root_a);

        pump_pending_diagnostic_publications(&server, &mut workspace)
            .expect("queued URI must be revalidated against current owners");
        let Message::Notification(update) = client
            .receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("current aggregate")
        else {
            panic!("expected current aggregate notification");
        };
        assert_eq!(update.method, "textDocument/publishDiagnostics");
        assert_eq!(update.params["diagnostics"].as_array().unwrap().len(), 1);
        assert_eq!(update.params["diagnostics"][0]["message"], "B current");
        let Message::Notification(warning) = client
            .receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("stale-owner incomplete notice")
        else {
            panic!("expected stale-owner warning notification");
        };
        assert_eq!(warning.method, "window/showMessage");
    }

    #[test]
    fn rapid_refresh_discards_writer_queued_old_version_before_resuming() {
        let temp = tempfile::tempdir().expect("workspace");
        let mut workspace = test_workspace(
            vec![temp.path().to_path_buf()],
            crate::workspace::WorkspaceOptions::default(),
        );
        let root = Url::from_file_path(temp.path().join("A.pas")).expect("root URI");
        let target = Url::from_file_path(temp.path().join("Shared.inc")).expect("target URI");
        let (outbound_sender, outbound_receiver) = crossbeam_channel::bounded(1);
        outbound_sender
            .send(Message::Notification(Notification::new(
                "$/writerPaused".into(),
                serde_json::Value::Null,
            )))
            .expect("pause transport writer");
        let (_inbound_sender, inbound_receiver) = crossbeam_channel::unbounded();
        let protocol = super::ProtocolConnection::new(
            Connection {
                sender: outbound_sender,
                receiver: inbound_receiver,
            },
            crossbeam_channel::unbounded().1,
            &TestBarrierConfig::disabled(),
        );
        let publication = |message: &str| DiagnosticPublication {
            uri: target.clone(),
            version: None,
            diagnostics: vec![Diagnostic::new_simple(Range::default(), message.into())],
        };
        workspace
            .stage_diagnostic_publications(&root, [publication("version one")])
            .expect("stage version one");
        pump_pending_diagnostic_publications(&protocol, &mut workspace)
            .expect("writer queue accepts bounded old output");
        assert!(protocol.has_pending_output());

        super::mark_publication_root_stale(&protocol, &mut workspace, &root);
        workspace
            .stage_diagnostic_publications(&root, [publication("version two")])
            .expect("stage newer generation");
        pump_pending_diagnostic_publications(&protocol, &mut workspace)
            .expect("new report remains resumable while writer is paused");

        assert!(matches!(
            outbound_receiver.try_recv(),
            Ok(Message::Notification(notification)) if notification.method == "$/writerPaused"
        ));
        protocol.flush().expect("resume transport writer");
        let Message::Notification(update) = outbound_receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("resumed latest report")
        else {
            panic!("expected latest diagnostics notification");
        };
        assert_eq!(update.method, "textDocument/publishDiagnostics");
        assert_eq!(update.params["diagnostics"][0]["message"], "version two");
        assert!(
            outbound_receiver.try_recv().is_err(),
            "old version was discarded"
        );
    }

    #[test]
    fn paused_writer_resumes_normal_push_without_losing_or_reordering_targets() {
        const TARGETS: usize = 70;
        let temp = tempfile::tempdir().expect("workspace");
        let mut workspace = test_workspace(
            vec![temp.path().to_path_buf()],
            crate::workspace::WorkspaceOptions::default(),
        );
        let root = Url::from_file_path(temp.path().join("A.pas")).expect("root URI");
        let expected = (0..TARGETS)
            .map(|index| {
                Url::from_file_path(temp.path().join(format!("Related{index:02}.inc")))
                    .expect("related URI")
            })
            .collect::<std::collections::HashSet<_>>();
        workspace
            .stage_diagnostic_publications(
                &root,
                expected.iter().cloned().map(|uri| DiagnosticPublication {
                    uri,
                    version: None,
                    diagnostics: vec![Diagnostic::new_simple(
                        Range::default(),
                        "paused-writer finding".into(),
                    )],
                }),
            )
            .expect("stage fanout");

        let (outbound_sender, outbound_receiver) = crossbeam_channel::bounded(1);
        let (_inbound_sender, inbound_receiver) = crossbeam_channel::unbounded();
        let connection = Connection {
            sender: outbound_sender,
            receiver: inbound_receiver,
        };
        let (_priority_sender, priority_receiver) = crossbeam_channel::unbounded();
        let protocol = super::ProtocolConnection::new(
            connection,
            priority_receiver,
            &TestBarrierConfig::disabled(),
        );

        let mut received = std::collections::HashSet::new();
        while workspace.pending_diagnostic_publication_count() > 0 || protocol.has_pending_output()
        {
            if workspace.pending_diagnostic_publication_count() > 0 {
                pump_pending_diagnostic_publications(&protocol, &mut workspace)
                    .expect("paused writer pressure remains resumable");
            }
            protocol.flush().expect("flush admitted output");
            while let Ok(message) = outbound_receiver.try_recv() {
                let Message::Notification(notification) = message else {
                    panic!("expected publication notification");
                };
                let uri = Url::parse(notification.params["uri"].as_str().unwrap())
                    .expect("publication URI");
                assert!(expected.contains(&uri));
                assert!(received.insert(uri), "duplicate publication after resume");
                protocol.flush().expect("resume queued writer output");
            }
        }
        assert_eq!(received, expected);
        assert_eq!(workspace.pending_diagnostic_publication_count(), 0);
    }

    #[test]
    fn event_loop_services_a_request_between_normal_push_batches() {
        const TARGETS: usize = 70;
        let temp = tempfile::tempdir().expect("workspace");
        let mut workspace = test_workspace(
            vec![temp.path().to_path_buf()],
            crate::workspace::WorkspaceOptions::default(),
        );
        let root = Url::from_file_path(temp.path().join("A.pas")).expect("root URI");
        let publications = (0..TARGETS)
            .map(|index| DiagnosticPublication {
                uri: Url::from_file_path(temp.path().join(format!("Related{index:02}.inc")))
                    .expect("related URI"),
                version: None,
                diagnostics: vec![Diagnostic::new_simple(
                    Range::default(),
                    format!("finding {index}"),
                )],
            })
            .collect::<Vec<_>>();
        workspace
            .stage_diagnostic_publications(&root, publications)
            .expect("stage normal push fanout");

        let (server, client) = Connection::memory();
        let (priority_sender, priority_receiver) = crossbeam_channel::unbounded();
        drop(priority_sender);
        let protocol = super::ProtocolConnection::new(
            server,
            priority_receiver,
            &TestBarrierConfig::disabled(),
        );
        let request_id = RequestId::from("responsive-request".to_string());
        client
            .sender
            .send(Message::Request(lsp_server::Request::new(
                request_id.clone(),
                "pascal/unknownProbe".to_string(),
                serde_json::json!({}),
            )))
            .expect("send request");
        client
            .sender
            .send(Message::Request(lsp_server::Request::new(
                RequestId::from("shutdown".to_string()),
                "shutdown".to_string(),
                serde_json::json!(null),
            )))
            .expect("send shutdown");
        client
            .sender
            .send(Message::Notification(Notification::new(
                "exit".to_string(),
                serde_json::json!(null),
            )))
            .expect("send exit");

        let mut configuration =
            ConfigurationCoordinator::new(None, WorkspaceOptions::default(), false, false);
        let result = super::event_loop(
            &protocol,
            &mut workspace,
            false,
            symbol_client_features(),
            false,
            false,
            false,
            &mut configuration,
            None,
            AnalysisJobs::new(),
        )
        .expect("event loop completes");
        assert!(result);

        let mut before_response = 0usize;
        let mut response_seen = false;
        let mut total_publications = 0usize;
        while let Ok(message) = client.receiver.try_recv() {
            match message {
                Message::Notification(notification)
                    if notification.method == "textDocument/publishDiagnostics" =>
                {
                    total_publications += 1;
                    if !response_seen {
                        before_response += 1;
                    }
                }
                Message::Response(response) if response.id == request_id => {
                    assert!(
                        response.error.is_some(),
                        "unknown request should be answered"
                    );
                    response_seen = true;
                }
                _ => {}
            }
        }
        assert!(response_seen);
        assert_eq!(total_publications, TARGETS);
        assert!(
            before_response <= 64,
            "request must interleave before second batch"
        );
    }

    #[test]
    fn staged_publication_retains_front_target_after_backpressure() {
        let temp = tempfile::tempdir().expect("workspace");
        let mut workspace = test_workspace(
            vec![temp.path().to_path_buf()],
            crate::workspace::WorkspaceOptions::default(),
        );
        let root = Url::from_file_path(temp.path().join("A.pas")).expect("root URI");
        let target = Url::from_file_path(temp.path().join("Related.inc")).expect("target URI");
        let sender = BackpressureOnceSender {
            should_block: AtomicBool::new(true),
            accepted: Mutex::new(Vec::new()),
        };
        let (connection, _client) = Connection::memory();
        super::send_diagnostic_publications(
            &connection,
            &mut workspace,
            &root,
            vec![DiagnosticPublication {
                uri: target.clone(),
                version: None,
                diagnostics: vec![Diagnostic::new_simple(
                    Range::default(),
                    "must be delivered".into(),
                )],
            }],
        )
        .expect("stage publication");

        pump_pending_diagnostic_publications(&sender, &mut workspace)
            .expect("temporary backpressure is retryable");
        assert_eq!(workspace.pending_diagnostic_publication_count(), 1);
        assert!(sender.accepted.lock().expect("sender lock").is_empty());

        pump_pending_diagnostic_publications(&sender, &mut workspace)
            .expect("resume the same target");
        assert_eq!(workspace.pending_diagnostic_publication_count(), 0);
        let accepted = sender.accepted.lock().expect("sender lock");
        assert_eq!(accepted.len(), 1);
        let Message::Notification(notification) = &accepted[0] else {
            panic!("expected publishDiagnostics");
        };
        assert_eq!(notification.params["uri"], target.as_str());
        assert_eq!(
            notification.params["diagnostics"][0]["message"],
            "must be delivered"
        );
    }

    #[test]
    fn completed_diagnostic_job_cannot_publish_ahead_of_pending_cleanup() {
        let temp = tempfile::tempdir().expect("workspace");
        let mut workspace = test_workspace(
            vec![temp.path().to_path_buf()],
            crate::workspace::WorkspaceOptions::default(),
        );
        let root = Url::from_file_path(temp.path().join("A.pas")).expect("root URI");
        workspace
            .open_document(
                root.clone(),
                "unit A;\ninterface\nimplementation\nend.\n".to_string(),
                1,
            )
            .expect("open owner document");
        let target = Url::from_file_path(temp.path().join("Z-target.inc")).expect("target URI");
        let publication = |message: &str| DiagnosticPublication {
            uri: target.clone(),
            version: None,
            diagnostics: vec![Diagnostic::new_simple(Range::default(), message.into())],
        };
        let sender = BackpressureOnceSender {
            should_block: AtomicBool::new(false),
            accepted: Mutex::new(Vec::new()),
        };

        super::send_diagnostic_publications(
            &sender,
            &mut workspace,
            &root,
            vec![publication("old diagnostic")],
        )
        .expect("stage old report");
        pump_pending_diagnostic_publications(&sender, &mut workspace)
            .expect("deliver old report before rejected-open cleanup");
        sender.accepted.lock().expect("sender lock").clear();

        let rejected_uri = Url::parse("file:///tmp/rejected-open.pas").expect("rejected URI");
        let cursor = workspace.take_all_diagnostic_publication_uris(Some(rejected_uri));
        let mut cleanup = super::PendingDiagnosticClears::default();
        cleanup.enqueue_cursor(cursor, None);
        let mut diagnostic_budget = super::DiagnosticPublicationTurnBudget::default();
        sender
            .should_block
            .store(true, std::sync::atomic::Ordering::Release);
        cleanup
            .pump(&sender, &mut diagnostic_budget)
            .expect("paused writer retains cleanup front");
        assert!(!cleanup.is_empty(), "cleanup cursor must remain active");

        let mut jobs = AnalysisJobs::new();
        let canceled_job_id = AnalysisComputationId(9000);
        let cancellation = Arc::new(AtomicBool::new(true));
        let handle = thread::spawn(|| {});
        jobs.diagnostics.insert(
            canceled_job_id,
            super::PendingDiagnostic {
                uri: root.clone(),
                analysis: PendingAnalysis {
                    cancellation,
                    handle,
                    recipients: Vec::new(),
                    key: None,
                },
            },
        );
        jobs.diagnostic_jobs.insert(root.clone(), canceled_job_id);
        jobs.sender
            .send(AnalysisResult {
                id: AnalysisJobId::Diagnostic(canceled_job_id),
                source_generation: workspace.source_generation(),
                configuration_generation: workspace.configuration_generation(),
                records: Vec::new(),
                value: AnalysisResultValue::Diagnostics(super::DiagnosticsAnalysis {
                    uri: root.clone(),
                    version: Some(1),
                    value: Ok(vec![publication("cancelled diagnostic")]),
                    discard: false,
                }),
            })
            .expect("queue cancelled diagnostic result");
        jobs.poll_with_diagnostic_budget(
            &sender,
            &mut workspace,
            &mut super::DiagnosticPublicationTurnBudget::default(),
            true,
        )
        .expect("poll cancelled diagnostic job");
        assert!(
            sender.accepted.lock().expect("sender lock").is_empty(),
            "cancelled job must not publish while cleanup is pending"
        );

        // Model a diagnostic worker that completed successfully after a close
        // released the rejected-open fence, while the old URI cursor is still
        // waiting for writer capacity.
        let job_id = AnalysisComputationId(9001);
        let cancellation = Arc::new(AtomicBool::new(false));
        let handle = thread::spawn(|| {});
        jobs.diagnostics.insert(
            job_id,
            super::PendingDiagnostic {
                uri: root.clone(),
                analysis: PendingAnalysis {
                    cancellation,
                    handle,
                    recipients: Vec::new(),
                    key: None,
                },
            },
        );
        jobs.diagnostic_jobs.insert(root.clone(), job_id);
        jobs.sender
            .send(AnalysisResult {
                id: AnalysisJobId::Diagnostic(job_id),
                source_generation: workspace.source_generation(),
                configuration_generation: workspace.configuration_generation(),
                records: Vec::new(),
                value: AnalysisResultValue::Diagnostics(super::DiagnosticsAnalysis {
                    uri: root,
                    version: Some(1),
                    value: Ok(vec![publication("fresh diagnostic")]),
                    discard: false,
                }),
            })
            .expect("queue completed diagnostic result");
        jobs.poll_with_diagnostic_budget(&sender, &mut workspace, &mut diagnostic_budget, false)
            .expect("poll completed job under cleanup precedence");

        let early_messages = sender.accepted.lock().expect("sender lock").clone();
        assert!(
            early_messages.is_empty(),
            "normal diagnostic output must wait until cleanup drains: {early_messages:?}"
        );

        while !cleanup.is_empty() {
            let mut drain_budget = super::DiagnosticPublicationTurnBudget::default();
            cleanup
                .pump(&sender, &mut drain_budget)
                .expect("drain pending cleanup");
            if cleanup.is_empty() {
                super::pump_pending_diagnostic_publications_with_budget(
                    &sender,
                    &mut workspace,
                    &mut drain_budget,
                )
                .expect("reaggregate current owners after cleanup");
            }
        }
        let accepted = sender.accepted.lock().expect("sender lock");
        let target_publications = accepted
            .iter()
            .filter_map(|message| match message {
                Message::Notification(notification)
                    if notification.method == "textDocument/publishDiagnostics"
                        && notification.params["uri"] == target.as_str() =>
                {
                    Some(notification.params.clone())
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(target_publications.len(), 2);
        assert!(
            target_publications[0]["diagnostics"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            target_publications[1]["diagnostics"][0]["message"],
            "fresh diagnostic"
        );
    }

    #[test]
    fn cleanup_and_normal_push_share_one_notification_and_byte_budget() {
        const TARGETS: usize = 72;
        let temp = tempfile::tempdir().expect("workspace");
        let mut workspace = test_workspace(
            vec![temp.path().to_path_buf()],
            crate::workspace::WorkspaceOptions::default(),
        );
        let root = Url::from_file_path(temp.path().join("A.pas")).expect("root URI");
        let targets = (0..TARGETS)
            .map(|index| {
                Url::parse(&format!("file:///tmp/push-budget-{index:03}.inc")).expect("target URI")
            })
            .collect::<Vec<_>>();
        let publication_set = |message: &str| {
            targets
                .iter()
                .map(|uri| DiagnosticPublication {
                    uri: uri.clone(),
                    version: None,
                    diagnostics: vec![Diagnostic::new_simple(
                        Range::default(),
                        message.to_string(),
                    )],
                })
                .collect::<Vec<_>>()
        };
        let sender = BackpressureOnceSender {
            should_block: AtomicBool::new(false),
            accepted: Mutex::new(Vec::new()),
        };
        super::send_diagnostic_publications(
            &sender,
            &mut workspace,
            &root,
            publication_set(&"x".repeat(18_500)),
        )
        .expect("stage retained root reports");
        while workspace.pending_diagnostic_publication_count() != 0 {
            pump_pending_diagnostic_publications(&sender, &mut workspace)
                .expect("drain initial root reports");
        }
        sender.accepted.lock().expect("sender lock").clear();

        let cursor = workspace.take_all_diagnostic_publication_uris(None);
        let mut cleanup = super::PendingDiagnosticClears::default();
        cleanup.enqueue_cursor(cursor, None);
        super::send_diagnostic_publications(
            &sender,
            &mut workspace,
            &root,
            publication_set(&"y".repeat(18_500)),
        )
        .expect("stage newer owner results while clear cursor is active");

        let mut budget = super::DiagnosticPublicationTurnBudget::default();
        cleanup
            .pump(&sender, &mut budget)
            .expect("pump cleanup batch");
        super::pump_pending_diagnostic_publications_with_budget(
            &sender,
            &mut workspace,
            &mut budget,
        )
        .expect("pump normal-publication batch under shared budget");

        let accepted = sender.accepted.lock().expect("sender lock");
        let notifications = accepted
            .iter()
            .filter(|message| {
                matches!(
                    message,
                    Message::Notification(notification)
                        if notification.method == "textDocument/publishDiagnostics"
                )
            })
            .count();
        let bytes = accepted
            .iter()
            .map(|message| {
                serde_json::to_vec(message)
                    .expect("serialize admitted notification")
                    .len()
                    .saturating_add(super::LSP_FRAME_HEADER_RESERVE_BYTES)
            })
            .sum::<usize>();
        assert!(
            notifications <= super::MAX_PUSH_DIAGNOSTIC_NOTIFICATIONS_PER_TURN,
            "cleanup and normal push emitted {notifications} notifications in one turn"
        );
        assert!(
            bytes <= super::MAX_PUSH_DIAGNOSTIC_BYTES_PER_TURN,
            "cleanup and normal push emitted {bytes} framed bytes in one turn"
        );
    }

    #[test]
    fn closing_owner_cancels_queued_finding_and_stages_an_empty_clear() {
        let temp = tempfile::tempdir().expect("workspace");
        let mut workspace = test_workspace(
            vec![temp.path().to_path_buf()],
            crate::workspace::WorkspaceOptions::default(),
        );
        let root = Url::from_file_path(temp.path().join("A.pas")).expect("root URI");
        let target = Url::from_file_path(temp.path().join("Related.inc")).expect("target URI");
        let (server, client) = Connection::memory();
        super::send_diagnostic_publications(
            &server,
            &mut workspace,
            &root,
            vec![DiagnosticPublication {
                uri: target.clone(),
                version: None,
                diagnostics: vec![Diagnostic::new_simple(
                    Range::default(),
                    "queued but superseded by close".into(),
                )],
            }],
        )
        .expect("stage owner report");
        workspace.stage_clear_diagnostic_publications(&root);

        pump_pending_diagnostic_publications(&server, &mut workspace)
            .expect("publish close cleanup");
        let mut cleared = std::collections::HashSet::new();
        for _ in 0..2 {
            let Message::Notification(notification) = client
                .receiver
                .recv_timeout(Duration::from_secs(1))
                .expect("close cleanup notification")
            else {
                panic!("expected a cleanup notification");
            };
            assert_eq!(notification.method, "textDocument/publishDiagnostics");
            assert_eq!(notification.params["diagnostics"], serde_json::json!([]));
            cleared.insert(notification.params["uri"].as_str().unwrap().to_owned());
        }
        assert!(cleared.contains(root.as_str()));
        assert!(cleared.contains(target.as_str()));
        assert_eq!(workspace.pending_diagnostic_publication_count(), 0);
        assert!(client.receiver.try_recv().is_err());
    }

    #[test]
    fn oversized_aggregate_is_omitted_with_operational_warning_not_empty_report() {
        let temp = tempfile::tempdir().expect("workspace");
        let mut workspace = test_workspace(
            vec![temp.path().to_path_buf()],
            crate::workspace::WorkspaceOptions::default(),
        );
        let target = Url::from_file_path(temp.path().join("Shared.inc")).expect("target URI");
        let (server, client) = Connection::memory();
        for owner_index in 0..2 {
            let root = Url::from_file_path(temp.path().join(format!("Owner{owner_index}.pas")))
                .expect("root URI");
            super::send_diagnostic_publications(
                &server,
                &mut workspace,
                &root,
                vec![DiagnosticPublication {
                    uri: target.clone(),
                    version: None,
                    diagnostics: (0..24)
                        .map(|index| {
                            Diagnostic::new_simple(
                                Range::default(),
                                format!("{}-{owner_index}-{index}", "x".repeat(1_500)),
                            )
                        })
                        .collect(),
                }],
            )
            .expect("stage current owner contribution");
        }

        pump_pending_diagnostic_publications(&server, &mut workspace)
            .expect("oversized report is skipped, not sent partially");
        let Message::Notification(warning) = client
            .receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("incomplete warning")
        else {
            panic!("expected operational warning");
        };
        assert_eq!(warning.method, "window/showMessage");
        assert!(
            warning.params["message"]
                .as_str()
                .unwrap()
                .contains("incomplete")
        );
        assert!(client.receiver.try_recv().is_err());
        assert_eq!(workspace.pending_diagnostic_publication_count(), 0);
    }

    #[test]
    fn staged_publication_turns_respect_count_and_serialized_byte_budgets() {
        const TARGETS: usize = 80;
        let temp = tempfile::tempdir().expect("workspace");
        let mut workspace = test_workspace(
            vec![temp.path().to_path_buf()],
            crate::workspace::WorkspaceOptions::default(),
        );
        let root = Url::from_file_path(temp.path().join("A.pas")).expect("root URI");
        let expected = (0..TARGETS)
            .map(|index| {
                Url::from_file_path(temp.path().join(format!("Related{index:02}.inc")))
                    .expect("related URI")
            })
            .collect::<std::collections::HashSet<_>>();
        let long_message = "x".repeat(20 * 1024);
        let publications = expected
            .iter()
            .cloned()
            .map(|uri| DiagnosticPublication {
                uri,
                version: None,
                diagnostics: vec![Diagnostic::new_simple(
                    Range::default(),
                    long_message.clone(),
                )],
            })
            .collect::<Vec<_>>();
        let (server, client) = Connection::memory();
        super::send_diagnostic_publications(&server, &mut workspace, &root, publications)
            .expect("stage high-fanout report");

        let mut received = std::collections::HashSet::new();
        while workspace.pending_diagnostic_publication_count() > 0 {
            pump_pending_diagnostic_publications(&server, &mut workspace)
                .expect("pump bounded notification turn");
            let mut turn_count = 0usize;
            let mut turn_bytes = 0usize;
            while let Ok(message) = client.receiver.try_recv() {
                let encoded = serde_json::to_vec(&message).expect("serialize test output");
                let wire_bytes = encoded.len() + super::LSP_FRAME_HEADER_RESERVE_BYTES;
                assert!(wire_bytes <= super::MAX_PUSH_DIAGNOSTIC_NOTIFICATION_BYTES);
                turn_bytes += wire_bytes;
                turn_count += 1;
                let Message::Notification(notification) = message else {
                    panic!("expected publishDiagnostics notification");
                };
                let uri = Url::parse(notification.params["uri"].as_str().unwrap())
                    .expect("published URI");
                assert!(received.insert(uri), "target emitted twice");
            }
            assert!(turn_count <= 64);
            assert!(turn_bytes <= super::MAX_PUSH_DIAGNOSTIC_BYTES_PER_TURN);
            assert!(turn_count > 0, "bounded byte budget must make progress");
        }
        assert_eq!(received, expected);
        assert!(client.receiver.try_recv().is_err());
    }

    #[test]
    fn open_document_count_rejection_cannot_requeue_an_emitted_cleanup_uri() {
        let temp = tempfile::tempdir().expect("workspace");
        let mut workspace = test_workspace(
            vec![temp.path().to_path_buf()],
            crate::workspace::WorkspaceOptions::default(),
        );
        let related_uri =
            Url::from_file_path(temp.path().join("aaa-related.pas")).expect("related URI");
        let root_uri = Url::from_file_path(temp.path().join("open-00000.pas")).expect("root URI");
        for index in 0..crate::workspace::MAX_OPEN_DOCUMENTS {
            let uri = if index == 0 {
                root_uri.clone()
            } else {
                Url::from_file_path(temp.path().join(format!("open-{index:05}.pas")))
                    .expect("open URI")
            };
            workspace.seed_open_document_for_test(uri);
        }
        workspace
            .replace_diagnostic_publications(
                &root_uri,
                [DiagnosticPublication {
                    uri: related_uri.clone(),
                    version: None,
                    diagnostics: Vec::new(),
                }],
            )
            .expect("retain an earlier publication");

        let cursor = workspace.take_all_diagnostic_publication_uris(None);
        let mut pending = super::PendingDiagnosticClears::default();
        pending.enqueue_cursor(cursor, None);
        let mut initial_count = 0;
        loop {
            let step = pending.cursor.as_mut().expect("cleanup cursor").next_step();
            match step {
                crate::workspace::DiagnosticPublicationCursorStep::Target(uri)
                    if uri == related_uri =>
                {
                    initial_count += 1;
                    break;
                }
                crate::workspace::DiagnosticPublicationCursorStep::Target(_)
                | crate::workspace::DiagnosticPublicationCursorStep::Skipped => {}
                crate::workspace::DiagnosticPublicationCursorStep::Exhausted => {
                    panic!("related publication was not found in the cleanup cursor")
                }
            }
        }
        assert_eq!(initial_count, 1);

        let effect = super::handle_notification_with_cancel(
            &super::UnusedProtocolSender,
            &mut workspace,
            lsp_server::Notification::new(
                "textDocument/didOpen".to_string(),
                serde_json::json!({
                    "textDocument": {
                        "uri": related_uri,
                        "languageId": "pascal",
                        "version": 2,
                        "text": "unit Related; interface implementation end."
                    }
                }),
            ),
            false,
            true,
            None,
            true,
        )
        .expect("count-cap rejection schedules cleanup rather than exiting");
        assert!(workspace.analysis_admission_fenced());
        assert_eq!(effect.cleanup_rejected_uri, Some(related_uri.clone()));
        pending.enqueue_cursor(
            effect
                .clear_publication_cursor
                .expect("count rejection returns its cleanup cursor"),
            effect.cleanup_rejected_uri,
        );

        let mut final_count = initial_count;
        let mut steps = 0usize;
        loop {
            steps += 1;
            assert!(steps < 25_000, "cleanup cursor did not drain bounded input");
            match pending
                .cursor
                .as_mut()
                .expect("active cleanup cursor")
                .next_step()
            {
                crate::workspace::DiagnosticPublicationCursorStep::Target(uri)
                    if uri == related_uri =>
                {
                    final_count += 1;
                }
                crate::workspace::DiagnosticPublicationCursorStep::Target(_)
                | crate::workspace::DiagnosticPublicationCursorStep::Skipped => {}
                crate::workspace::DiagnosticPublicationCursorStep::Exhausted => break,
            }
        }
        assert_eq!(
            final_count, 1,
            "the count-cap late target must not duplicate an already-emitted URI"
        );
    }

    fn symbol_client_features() -> ClientFeatures {
        ClientFeatures {
            action_resolve: false,
            action_disabled: false,
            document_changes: false,
            rename_file: false,
            will_rename_files: false,
            hierarchical_document_symbols: false,
            hover_format: DocumentationFormat::PlainText,
            completion_format: DocumentationFormat::PlainText,
            completion_snippet_support: false,
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

    #[test]
    fn server_advertises_workspace_file_operations() {
        let capabilities = super::server_capabilities(&ClientCapabilities::default(), false);
        let file_operations = &capabilities["workspace"]["fileOperations"];
        for operation in [
            "willCreate",
            "willRename",
            "willDelete",
            "didCreate",
            "didRename",
            "didDelete",
        ] {
            assert!(file_operations[operation].is_array(), "missing {operation}");
            assert_eq!(file_operations[operation][0]["pattern"]["matches"], "file");
            assert_eq!(
                file_operations[operation][0]["pattern"]["glob"],
                "**/*.{pas,pp,pascal}"
            );
        }
        assert!(super::notification_requires_configuration_ordering(
            "workspace/didRenameFiles"
        ));
        assert!(super::notification_may_change_document(
            "workspace/didRenameFiles"
        ));
    }

    #[test]
    fn diagnostic_refresh_support_accepts_the_plural_protocol_capability_name() {
        let raw_initialize = serde_json::json!({
            "capabilities": {
                "workspace": {
                    "diagnostics": {"refreshSupport": true}
                }
            }
        });
        let capabilities: ClientCapabilities =
            serde_json::from_value(raw_initialize["capabilities"].clone())
                .expect("client capabilities");
        assert!(supports_diagnostic_refresh(&capabilities, &raw_initialize));
    }

    #[test]
    fn pull_owned_receive_timeout_ignores_expired_push_deadlines() {
        assert_eq!(
            event_loop_receive_timeout(true, Some(Duration::ZERO), true, false, false,),
            Duration::from_secs(86_400)
        );
        assert_eq!(
            event_loop_receive_timeout(false, Some(Duration::ZERO), true, false, false,),
            Duration::ZERO
        );
    }

    #[test]
    fn diagnostic_provider_supports_standard_and_legacy_workspace_capabilities() {
        let raw_initialize = serde_json::json!({
            "capabilities": {
                "textDocument": {"diagnostic": {"relatedDocumentSupport": true}},
                "workspace": {
                    "diagnostics": {"refreshSupport": true}
                }
            }
        });
        assert!(supports_workspace_diagnostic_reports(&raw_initialize));
        assert!(supports_workspace_diagnostic_reports(&serde_json::json!({
            "capabilities": {
                "textDocument": {"diagnostic": {}},
                "workspace": {
                    "diagnostic": {"refreshSupport": true}
                }
            }
        })));
        assert!(!supports_workspace_diagnostic_reports(&serde_json::json!({
            "capabilities": {
                "textDocument": {"diagnostic": {}}
            }
        })));
        assert!(!supports_workspace_diagnostic_reports(&serde_json::json!({
            "clientInfo": {"name": "Neovim", "version": "0.12.5"},
            "capabilities": {
                "textDocument": {"diagnostic": {}},
                "workspace": {
                    "diagnostics": {"refreshSupport": true}
                }
            }
        })));
        assert!(supports_workspace_diagnostic_reports(&serde_json::json!({
            "clientInfo": {"name": "Neovim", "version": "0.12.5-dev"},
            "capabilities": {
                "workspace": {
                    "diagnostics": {"refreshSupport": true}
                }
            }
        })));
        assert!(supports_workspace_diagnostic_reports(&serde_json::json!({
            "clientInfo": {"name": "Neovim", "version": "0.12.50"},
            "capabilities": {
                "workspace": {
                    "diagnostics": {"refreshSupport": true}
                }
            }
        })));
        assert!(!supports_workspace_diagnostic_reports(&serde_json::json!({
            "clientInfo": {"name": "Neovim", "version": "0.12.5+v0.12.5"},
            "capabilities": {
                "workspace": {
                    "diagnostics": {"refreshSupport": true}
                }
            }
        })));
        assert!(!supports_workspace_diagnostic_reports(&serde_json::json!({
            "clientInfo": {"name": "Neovim", "version": "v0.12.5+v0.12.5"},
            "capabilities": {
                "workspace": {
                    "diagnostics": {"refreshSupport": true}
                }
            }
        })));
        assert!(supports_workspace_diagnostic_reports(&serde_json::json!({
            "clientInfo": {"name": "Neovim", "version": "0.12.5+patched"},
            "capabilities": {
                "workspace": {
                    "diagnostics": {"refreshSupport": true}
                }
            }
        })));
        assert!(supports_workspace_diagnostic_reports(&serde_json::json!({
            "clientInfo": {"name": "neovim", "version": "0.12.5"},
            "capabilities": {
                "workspace": {
                    "diagnostics": {"refreshSupport": true}
                }
            }
        })));
        assert!(supports_workspace_diagnostic_reports(&serde_json::json!({
            "clientInfo": {"name": "Neovim", "version": "0.12.6"},
            "capabilities": {
                "workspace": {
                    "diagnostics": {"refreshSupport": true}
                }
            }
        })));
        assert!(supports_workspace_diagnostic_reports(&serde_json::json!({
            "clientInfo": {"name": "Neovim", "version": "0.13.0"},
            "capabilities": {
                "workspace": {
                    "diagnostics": {"refreshSupport": true}
                }
            }
        })));
        assert!(supports_workspace_diagnostic_reports(&serde_json::json!({
            "clientInfo": {"name": "Neovim"},
            "capabilities": {
                "workspace": {
                    "diagnostics": {"refreshSupport": true}
                }
            }
        })));
    }

    fn test_completion_analysis(uri: &Url, index: usize) -> CompletionAnalysis {
        CompletionAnalysis {
            uri: uri.clone(),
            position: Position::new(0, 0),
            format: MarkupKind::PlainText,
            snippet_support: false,
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

    fn test_diagnostic(index: usize) -> Vec<Diagnostic> {
        vec![Diagnostic::new(
            Range::new(Position::new(0, 0), Position::new(0, 1)),
            Some(DiagnosticSeverity::WARNING),
            None,
            None,
            format!("diagnostic {index}"),
            None,
            None,
        )]
    }

    fn test_diagnostic_dependency() -> Option<super::DiagnosticDependency> {
        let record = SourceRecord {
            uri: Url::parse("file:///diagnostic-dependency.pas").expect("dependency URI"),
            text: String::new(),
            version: Some(1),
            stamp: None,
            open: true,
            path: None,
            path_stamp: None,
            content_hash: Some(0),
            parsed_text_hash: Some(0),
            content_bytes: None,
            candidate_membership: None,
            candidate_observations: Vec::new(),
            read_policy: None,
            path_entry: None,
            include_payload: false,
            missing_provider_candidate: false,
            document_link_missing_candidate: false,
            directory_observation: false,
            missing_provider_scope: None,
            auto_import_provider_observation: false,
            auto_import_scopes: Vec::new(),
        };
        Some(
            super::prepare_diagnostic_dependency(&[record], 0, 0)
                .expect("diagnostic dependency")
                .expect("non-empty diagnostic dependency"),
        )
    }

    #[test]
    fn diagnostic_result_store_evicts_old_ids_but_keeps_new_ids() {
        let mut store = DiagnosticPullStore::new();
        let empty_dependency = test_diagnostic_dependency();
        let mut first = None;
        let mut last = None;
        for index in 0..=super::MAX_DIAGNOSTIC_RESULT_ENTRIES {
            let uri =
                Url::parse(&format!("file:///diagnostic-{index}.pas")).expect("diagnostic URI");
            let entry = store
                .insert(uri, None, test_diagnostic(index), empty_dependency.as_ref())
                .expect("diagnostic result entry");
            if index == 0 {
                first = Some(entry.result_id.clone());
            }
            last = Some(entry.result_id);
        }
        assert!(
            store.get(&first.expect("first result ID")).is_none(),
            "old diagnostic IDs must be evicted"
        );
        assert!(
            store.get(&last.expect("last result ID")).is_some(),
            "new diagnostic IDs must remain available"
        );
    }

    #[test]
    fn diagnostic_result_store_reissues_an_actual_evicted_id() {
        let first_uri = Url::parse("file:///diagnostic-evicted.pas").expect("diagnostic URI");
        let mut store = DiagnosticPullStore::new();
        let empty_dependency = test_diagnostic_dependency();
        let first = store
            .insert(
                first_uri.clone(),
                None,
                test_diagnostic(0),
                empty_dependency.as_ref(),
            )
            .expect("first diagnostic result");
        for index in 1..=super::MAX_DIAGNOSTIC_RESULT_ENTRIES {
            let uri = Url::parse(&format!("file:///diagnostic-fill-{index}.pas"))
                .expect("diagnostic fill URI");
            store
                .insert(uri, None, test_diagnostic(index), empty_dependency.as_ref())
                .expect("diagnostic fill result");
        }
        assert!(
            store.get(&first.result_id).is_none(),
            "the original result must be gone from the bounded cache"
        );
        let replacement = store
            .insert(
                first_uri,
                None,
                test_diagnostic(0),
                empty_dependency.as_ref(),
            )
            .expect("replacement diagnostic result");
        assert_ne!(replacement.result_id, first.result_id);
        assert!(store.get(&replacement.result_id).is_some());
    }

    #[test]
    fn diagnostic_result_store_replacement_does_not_grow_eviction_order() {
        let uri = Url::parse("file:///diagnostic-replacement.pas").expect("diagnostic URI");
        let mut store = DiagnosticPullStore::new();
        let empty_dependency = test_diagnostic_dependency();
        for index in 0..64 {
            store
                .insert(
                    uri.clone(),
                    None,
                    test_diagnostic(index),
                    empty_dependency.as_ref(),
                )
                .expect("diagnostic replacement entry");
        }
        assert_eq!(store.entries.len(), 1);
        assert_eq!(store.order.len(), 1);
    }

    #[test]
    fn diagnostic_result_store_degrades_one_entry_over_the_byte_bound() {
        let uri = Url::parse("file:///diagnostic-oversized.pas").expect("diagnostic URI");
        let mut store = DiagnosticPullStore::new();
        let diagnostics = vec![Diagnostic::new(
            Range::new(Position::new(0, 0), Position::new(0, 1)),
            Some(DiagnosticSeverity::ERROR),
            None,
            None,
            "x".repeat(super::MAX_DIAGNOSTIC_RESULT_BYTES),
            None,
            None,
        )];
        let entry = store
            .insert(uri, None, diagnostics, None)
            .expect("oversized diagnostic result remains a valid full report");
        assert!(!entry.result_id.is_empty());
        assert!(store.entries.is_empty());
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
            candidate_observations: Vec::new(),
            read_policy: None,
            path_entry: None,
            include_payload: false,
            missing_provider_candidate: false,
            document_link_missing_candidate: false,
            directory_observation: false,
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
                snippet_support: false,
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

    #[allow(deprecated)]
    fn boundary_symbol(name_length: usize) -> SymbolInformation {
        SymbolInformation {
            name: "x".repeat(name_length),
            kind: SymbolKind::VARIABLE,
            tags: None,
            deprecated: None,
            location: Location {
                uri: Url::parse("file:///boundary.pas").expect("boundary URI"),
                range: Range::new(Position::new(0, 0), Position::new(0, 1)),
            },
            container_name: None,
        }
    }

    #[test]
    fn partial_chunk_accepts_the_exact_encoded_limit_and_rejects_one_byte_over() {
        let mut name_length = 0;
        while serde_json::to_vec(&boundary_symbol(name_length))
            .expect("symbol encoding")
            .len()
            + 2
            <= MAX_PARTIAL_RESULT_BYTES_PER_CHUNK
        {
            name_length += 1;
        }
        let at_limit = PartialResultPayload::WorkspaceSymbols(Arc::new(vec![boundary_symbol(
            name_length.saturating_sub(1),
        )]));
        let (_, value) = at_limit
            .chunk(0)
            .expect("boundary chunk encoding")
            .expect("boundary chunk");
        assert_eq!(
            serde_json::to_vec(&value)
                .expect("boundary array encoding")
                .len(),
            MAX_PARTIAL_RESULT_BYTES_PER_CHUNK
        );

        let over_limit =
            PartialResultPayload::WorkspaceSymbols(Arc::new(vec![boundary_symbol(name_length)]));
        let error = over_limit
            .chunk(0)
            .expect_err("one byte over the encoded chunk limit must fail");
        assert!(error.contains("chunk limit"));
    }

    #[test]
    fn bounded_outbound_queue_reserves_control_capacity_when_data_is_full() {
        let (sender, receiver) = bounded(1);
        let mut queue = OutboundQueue::default();
        let data = || {
            Message::Notification(Notification::new(
                "$/progress".to_string(),
                serde_json::json!({"token": "data", "value": [1]}),
            ))
        };
        for _ in 0..=MAX_PENDING_OUTBOUND_DATA_MESSAGES {
            assert!(
                queue
                    .enqueue(&sender, data(), OutboundClass::Data)
                    .expect("data enqueue")
            );
        }
        assert_eq!(
            queue.pending_data_messages,
            MAX_PENDING_OUTBOUND_DATA_MESSAGES
        );
        let control = Message::Response(Response::new_ok(
            RequestId::from("cancel".to_string()),
            serde_json::Value::Null,
        ));
        assert!(
            queue
                .enqueue(&sender, control, OutboundClass::Control)
                .expect("reserved control enqueue")
        );
        assert_eq!(queue.pending_control_messages, 1);
        assert!(receiver.try_recv().is_ok());
    }

    #[test]
    fn supersession_discards_queued_diagnostic_messages_and_recounts_limits() {
        let (sender, _receiver) = bounded(1);
        sender
            .send(Message::Notification(Notification::new(
                "$/occupied".to_string(),
                serde_json::Value::Null,
            )))
            .expect("occupy writer channel");
        let target = Url::parse("file:///tmp/queued.inc").expect("target URI");
        let unrelated = Url::parse("file:///tmp/other.inc").expect("unrelated URI");
        let publication = |uri: &Url, message: &str| {
            Message::Notification(Notification::new(
                "textDocument/publishDiagnostics".to_string(),
                serde_json::json!({"uri": uri, "diagnostics": [{"message": message}]}),
            ))
        };
        let mut queue = OutboundQueue::default();
        queue
            .enqueue(
                &sender,
                publication(&target, "stale"),
                OutboundClass::Control,
            )
            .expect("queue stale report");
        queue
            .enqueue(
                &sender,
                publication(&target, "new generation"),
                OutboundClass::Control,
            )
            .expect("queue current-generation report");
        queue
            .enqueue(
                &sender,
                publication(&unrelated, "still current"),
                OutboundClass::Control,
            )
            .expect("queue unrelated report");
        assert_eq!(queue.pending_control_messages, 3);
        queue.pending[1].diagnostic_generation = Some(u64::MAX);
        let superseded = std::collections::BTreeSet::from([target]);

        let scan = queue.discard_diagnostic_publications(&superseded);

        assert_eq!(scan.scanned_messages, 3);
        assert_eq!(scan.removed_messages, 1);
        assert_eq!(queue.pending_control_messages, 2);
        assert_eq!(queue.pending.len(), 2);
        assert_eq!(queue.deferred_control_messages, 0);
        let Message::Notification(notification) = &queue.pending[0].message else {
            panic!("expected remaining diagnostics notification");
        };
        assert_eq!(
            notification.params["diagnostics"][0]["message"],
            "new generation"
        );
        let Message::Notification(notification) = &queue.pending[1].message else {
            panic!("expected remaining diagnostics notification");
        };
        assert_eq!(notification.params["uri"], unrelated.as_str());
    }

    #[test]
    fn deferred_result_byte_boundary_accepts_exact_limit_and_defers_one_byte_over() {
        let (sender, receiver) = bounded(1);
        sender
            .send(Message::Notification(Notification::new(
                "$/occupied".to_string(),
                serde_json::Value::Null,
            )))
            .expect("occupy writer queue");
        let mut queue = OutboundQueue::default();
        let result = |id: &str, value| {
            Message::Response(Response::new_ok(RequestId::from(id.to_string()), value))
        };
        let first = result(
            "deferred-first",
            serde_json::Value::String("x".repeat(MAX_PENDING_OUTBOUND_CONTROL_BYTES / 2)),
        );
        let first_bytes = serde_json::to_vec(&first)
            .expect("first result encoding")
            .len();
        let second_overhead = serde_json::to_vec(&result(
            "deferred-second",
            serde_json::Value::String(String::new()),
        ))
        .expect("empty second result encoding")
        .len();
        let second = result(
            "deferred-second",
            serde_json::Value::String(
                "x".repeat(MAX_PENDING_OUTBOUND_CONTROL_BYTES - first_bytes - second_overhead),
            ),
        );
        let second_bytes = serde_json::to_vec(&second)
            .expect("second result encoding")
            .len();
        assert!(first_bytes < MAX_PENDING_OUTBOUND_CONTROL_BYTES);
        assert!(second_bytes < MAX_PENDING_OUTBOUND_CONTROL_BYTES);
        assert_eq!(
            first_bytes.saturating_add(second_bytes),
            MAX_PENDING_OUTBOUND_CONTROL_BYTES
        );

        let over = result("deferred-over", serde_json::Value::Null);
        let over_bytes = serde_json::to_vec(&over)
            .expect("over-limit result encoding")
            .len();
        assert!(
            first_bytes
                .saturating_add(second_bytes)
                .saturating_add(over_bytes)
                > MAX_PENDING_OUTBOUND_CONTROL_BYTES,
            "the third individually valid result must cross the aggregate pending boundary"
        );

        assert!(
            queue
                .enqueue(&sender, first, OutboundClass::Result)
                .expect("first result enqueue")
        );
        assert!(
            queue
                .enqueue(&sender, second, OutboundClass::Result)
                .expect("exact-boundary result enqueue")
        );
        assert_eq!(
            queue.pending_control_bytes,
            MAX_PENDING_OUTBOUND_CONTROL_BYTES
        );
        assert_eq!(queue.deferred_result_bytes(), 0);
        assert!(
            queue
                .enqueue(&sender, over, OutboundClass::Result)
                .expect("one-byte-over result deferral")
        );
        assert_eq!(queue.deferred_result_bytes(), over_bytes);
        assert!(queue.has_pending());

        assert!(matches!(
            receiver.recv().expect("occupied message"),
            Message::Notification(_)
        ));
        queue.flush(&sender).expect("flush first result");
        let first = receiver.recv().expect("first deferred-boundary result");
        match first {
            Message::Response(response) => {
                assert_eq!(response.id, RequestId::from("deferred-first".to_string()))
            }
            message => panic!("unexpected first deferred-boundary message: {message:?}"),
        }
        queue.flush(&sender).expect("flush exact-boundary result");
        let second = receiver.recv().expect("second deferred-boundary result");
        match second {
            Message::Response(response) => {
                assert_eq!(response.id, RequestId::from("deferred-second".to_string()))
            }
            message => panic!("unexpected second deferred-boundary message: {message:?}"),
        }
        queue.flush(&sender).expect("flush deferred result");
        let over = receiver.recv().expect("one-byte-over deferred result");
        match over {
            Message::Response(response) => {
                assert_eq!(response.id, RequestId::from("deferred-over".to_string()))
            }
            message => panic!("unexpected one-byte-over deferred message: {message:?}"),
        }
        assert_eq!(queue.deferred_result_bytes(), 0);
        assert!(!queue.has_pending());
    }

    #[test]
    fn deferred_output_keeps_result_and_control_count_reserves_separate() {
        let (sender, receiver) = bounded(1);
        sender
            .send(Message::Notification(Notification::new(
                "$/occupied".to_string(),
                serde_json::Value::Null,
            )))
            .expect("occupy writer queue");
        let mut queue = OutboundQueue::default();
        let control = || {
            Message::Notification(Notification::new(
                "$/control".to_string(),
                serde_json::Value::Null,
            ))
        };
        for _ in 0..MAX_PENDING_OUTBOUND_CONTROL_MESSAGES {
            assert!(
                queue
                    .enqueue(&sender, control(), OutboundClass::Control)
                    .expect("control enqueue")
            );
        }
        assert_eq!(
            queue.pending_control_messages,
            MAX_PENDING_OUTBOUND_CONTROL_MESSAGES
        );

        let result = Message::Response(Response::new_ok(
            RequestId::from("deferred-count-result".to_string()),
            serde_json::Value::Null,
        ));
        assert!(
            queue
                .enqueue(&sender, result, OutboundClass::Result)
                .expect("result deferral")
        );
        assert_eq!(queue.deferred_result_messages, 1);

        assert!(
            queue
                .enqueue(&sender, control(), OutboundClass::Control)
                .expect("control deferral")
        );
        assert_eq!(queue.deferred_control_messages, 1);

        let mut received = 0;
        while queue.has_pending() {
            queue.flush(&sender).expect("flush bounded count burst");
            if queue.has_pending() {
                receiver.recv().expect("receive bounded count burst");
                received += 1;
            }
        }
        while receiver.try_recv().is_ok() {
            received += 1;
        }
        assert_eq!(received, MAX_PENDING_OUTBOUND_CONTROL_MESSAGES + 3);
    }

    #[test]
    fn delivering_recipients_consume_the_global_admission_bound() {
        let temp = tempfile::tempdir().expect("workspace");
        let root = temp.path().to_path_buf();
        let source = root.join("Main.pas");
        fs::write(&source, "unit Main; interface implementation end.\n").expect("source");
        let workspace = test_workspace(vec![root], Default::default());
        let records = Arc::new(Vec::new());
        let validation = PartialDeliveryValidation::new(
            Arc::new(workspace.revalidation_input()),
            Arc::clone(&records),
            TestBarrierConfig::disabled(),
        )
        .expect("validation worker");
        let recipients = (0..MAX_CLIENT_ANALYSIS_RECIPIENTS)
            .map(|index| PartialDeliveryRecipient {
                id: RequestId::from(format!("delivering-{index}")),
                token: lsp_types::ProgressToken::String(format!("partial-{index}")),
                next_item: 0,
            })
            .collect();
        let mut jobs = AnalysisJobs::new();
        jobs.partial_deliveries.push_back(PartialDelivery {
            job_id: AnalysisComputationId(0),
            source_generation: workspace.source_generation(),
            configuration_generation: workspace.configuration_generation(),
            payload: PartialResultPayload::References(Arc::new(Vec::new())),
            retrigger_on_stale: false,
            recipients,
            next_recipient: 0,
            retained_bytes: 1,
            validation,
        });
        assert_eq!(
            jobs.client_recipient_count(),
            MAX_CLIENT_ANALYSIS_RECIPIENTS
        );
        let error = jobs
            .enqueue_client_with_partial(
                RequestId::from("after-delivery-bound".to_string()),
                AnalysisRequest::WorkspaceSymbols {
                    query: "Main".to_string(),
                },
                &workspace,
                symbol_client_features(),
                AnalysisProgressTokens::default(),
                None,
            )
            .expect_err("delivering recipients must count toward admission");
        assert_eq!(error, ANALYSIS_QUEUE_FULL_MESSAGE);
    }

    #[cfg(feature = "test-support")]
    #[test]
    fn retired_partial_validation_keeps_worker_and_bytes_until_reaped() {
        let temp = tempfile::tempdir().expect("workspace");
        let workspace = test_workspace(vec![temp.path().to_path_buf()], Default::default());
        let barrier_directory = temp.path().join("partial-validation-barrier");
        fs::create_dir_all(&barrier_directory).expect("barrier directory");
        let entered = barrier_directory.join("entered");
        let release = barrier_directory.join("release");
        let validation = PartialDeliveryValidation::new(
            Arc::new(workspace.revalidation_input()),
            Arc::new(Vec::new()),
            TestBarrierConfig::default()
                .with_partial_validation(Some((entered.clone(), release.clone()))),
        )
        .expect("validation worker");

        let deadline = Instant::now() + Duration::from_secs(5);
        while !entered.exists() {
            assert!(
                Instant::now() < deadline,
                "validation worker must reach the held test barrier"
            );
            thread::sleep(Duration::from_millis(5));
        }

        let mut jobs = AnalysisJobs::new();
        jobs.retire_partial_validation(validation, 1234);
        assert_eq!(jobs.client_recipient_count(), 1);
        assert_eq!(jobs.partial_delivery_bytes(), 1234);
        assert_eq!(jobs.retired_partial_validations.len(), 1);

        fs::write(&release, b"release").expect("release validation barrier");
        while !jobs.retired_partial_validations.is_empty() {
            jobs.reap_retired_partial_validations();
            assert!(
                Instant::now() < deadline,
                "retired validation worker must be reaped after release"
            );
            if !jobs.retired_partial_validations.is_empty() {
                thread::sleep(Duration::from_millis(5));
            }
        }
        assert_eq!(jobs.client_recipient_count(), 0);
        assert_eq!(jobs.partial_delivery_bytes(), 0);
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
                .recipients
                .iter()
                .map(|recipient| recipient.id.clone())
                .collect::<Vec<_>>(),
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
                snippet_support: false,
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
                snippet_support: false,
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
                recipients: Vec::new(),
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
                snippet_support: false,
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
