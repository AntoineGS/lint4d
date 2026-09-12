//! Synchronous stdio LSP protocol loop for the Pascal navigation workspace.

use crate::NavigationTarget;
use crate::workspace::codeactions::{self, ClientActionFeatures};
use crate::workspace::rename::{self, SourceRecord};
use crate::workspace::{
    FileChange, MAX_CONFIGURATION_WATCH_PATHS, Workspace, WorkspaceOptions, canonical_file_uri,
};
use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, bounded, unbounded};
use lsp_server::{Connection, ErrorCode, Message, Notification, Request, RequestId, Response};
use lsp_types::{
    ClientCapabilities, CodeAction, CodeActionOrCommand, CodeActionParams,
    DidChangeTextDocumentParams, DidChangeWatchedFilesParams, DidCloseTextDocumentParams,
    DidOpenTextDocumentParams, DidSaveTextDocumentParams, DocumentFormattingParams, FileChangeType,
    FileSystemWatcher, GlobPattern, GotoDefinitionParams, GotoDefinitionResponse, InitializeParams,
    OneOf, Position, PrepareRenameResponse, PublishDiagnosticsParams, Registration,
    RegistrationParams, RelativePattern, ServerInfo, TextDocumentIdentifier, Url, WatchKind,
    WorkspaceEdit, WorkspaceFolder,
};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::collections::HashSet;
use std::error::Error;
use std::io::{self, BufRead, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::thread::{self, JoinHandle};
use std::time::Duration;

const SERVER_NAME: &str = "pascal-lsp";
const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");
const MAX_PAYLOAD_BYTES: usize = 8 * 1024 * 1024;
const MAX_HEADER_BYTES: usize = 8 * 1024;
const MAX_ANALYSIS_JOBS: usize = 2;
const ANALYSIS_POLL_INTERVAL: Duration = Duration::from_millis(10);
const MAX_WATCHER_REGISTRATION_RETRIES: usize = 3;

#[derive(Debug, Clone, Copy)]
struct ClientFeatures {
    action_resolve: bool,
    action_disabled: bool,
    document_changes: bool,
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

enum AnalysisRequest {
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
}

enum AnalysisResultValue {
    Prepare(Result<PrepareRenameResponse, String>),
    Rename(Box<Result<WorkspaceEdit, String>>),
    CodeActions(Result<Vec<CodeActionOrCommand>, String>),
    Resolve(Box<Result<CodeAction, String>>),
}

struct AnalysisResult {
    id: RequestId,
    source_generation: u64,
    configuration_generation: u64,
    records: Vec<SourceRecord>,
    value: AnalysisResultValue,
}

struct PendingAnalysis {
    cancellation: Arc<AtomicBool>,
    handle: JoinHandle<()>,
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
    pending: std::collections::HashMap<RequestId, PendingAnalysis>,
}

impl AnalysisJobs {
    fn new() -> Self {
        let (sender, receiver) = unbounded();
        Self {
            sender,
            receiver,
            pending: std::collections::HashMap::new(),
        }
    }

    fn start(
        &mut self,
        id: RequestId,
        request: AnalysisRequest,
        workspace: &Workspace,
        features: ClientFeatures,
    ) -> Result<(), String> {
        if self.pending.len() >= MAX_ANALYSIS_JOBS {
            return Err("analysis server is busy; retry the request".to_string());
        }
        let input = workspace.analysis_input();
        let source_generation = input.source_generation;
        let configuration_generation = input.configuration_generation;
        let cancellation = Arc::new(AtomicBool::new(false));
        let worker_cancellation = Arc::clone(&cancellation);
        let sender = self.sender.clone();
        let worker_id = id.clone();
        let panic_id = id.clone();
        let handle = thread::Builder::new()
            .name("PascalLspAnalysis".to_string())
            .spawn(move || {
                let validation_input = input.clone();
                let result =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match request {
                        AnalysisRequest::Prepare { uri, position } => {
                            let computed = rename::prepare_from_input(
                                input,
                                &uri,
                                position,
                                &worker_cancellation,
                            );
                            AnalysisResult {
                                id: worker_id.clone(),
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
                                id: worker_id.clone(),
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
                                id: worker_id.clone(),
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
                                id: worker_id.clone(),
                                source_generation: computed.source_generation,
                                configuration_generation: computed.configuration_generation,
                                records: computed.records,
                                value: AnalysisResultValue::Resolve(Box::new(computed.value)),
                            }
                        }
                    }));
                let mut result = result.unwrap_or_else(|_| AnalysisResult {
                    id: panic_id,
                    source_generation,
                    configuration_generation,
                    records: Vec::new(),
                    value: AnalysisResultValue::Prepare(Err(
                        "analysis worker failed without changing workspace state".to_string(),
                    )),
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
        self.pending.insert(
            id,
            PendingAnalysis {
                cancellation,
                handle,
            },
        );
        Ok(())
    }

    fn cancel(&self, id: &RequestId) {
        if let Some(job) = self.pending.get(id) {
            job.cancellation
                .store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }

    fn poll(
        &mut self,
        connection: &Connection,
        workspace: &Workspace,
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        while let Ok(result) = self.receiver.try_recv() {
            if let Some(job) = self.pending.remove(&result.id) {
                let _ = job.handle.join();
            }
            deliver_analysis_result(connection, workspace, result)?;
        }
        Ok(())
    }

    fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    fn shutdown(&mut self) {
        let pending = std::mem::take(&mut self.pending);
        for (_, job) in pending {
            job.cancellation
                .store(true, std::sync::atomic::Ordering::Relaxed);
            let _ = job.handle.join();
        }
    }
}

fn deliver_analysis_result(
    connection: &Connection,
    workspace: &Workspace,
    result: AnalysisResult,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    if result.source_generation != workspace.source_generation()
        || result.configuration_generation != workspace.configuration_generation()
    {
        return send_error(
            connection,
            result.id,
            ErrorCode::RequestFailed,
            "analysis result became stale; retry the request",
        );
    }
    match result.value {
        AnalysisResultValue::Prepare(value) => match value {
            Ok(value) => send_ok(connection, result.id, value),
            Err(error) => send_analysis_error(connection, result.id, error),
        },
        AnalysisResultValue::Rename(value) => match *value {
            Ok(value) => send_ok(connection, result.id, value),
            Err(error) => send_analysis_error(connection, result.id, error),
        },
        AnalysisResultValue::CodeActions(value) => match value {
            Ok(value) => send_ok(connection, result.id, value),
            Err(error) => send_analysis_error(connection, result.id, error),
        },
        AnalysisResultValue::Resolve(value) => match *value {
            Ok(value) => send_ok(connection, result.id, value),
            Err(error) => send_analysis_error(connection, result.id, error),
        },
    }
}

fn invalidate_analysis_result(result: &mut AnalysisResult, error: String) {
    match &mut result.value {
        AnalysisResultValue::Prepare(value) => *value = Err(error),
        AnalysisResultValue::Rename(value) => **value = Err(error),
        AnalysisResultValue::CodeActions(value) => *value = Err(error),
        AnalysisResultValue::Resolve(value) => **value = Err(error),
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
    let (connection, io_threads) = bounded_stdio();
    let outcome = run_connection(&connection);
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

fn run_connection(connection: &Connection) -> Result<bool, Box<dyn Error + Send + Sync>> {
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
    )
}

fn event_loop(
    connection: &Connection,
    workspace: &mut Workspace,
    workspace_folders_supported: bool,
    client_features: ClientFeatures,
    mut watcher_registration: Option<FileWatcherRegistration>,
) -> Result<bool, Box<dyn Error + Send + Sync>> {
    let mut shutdown_received = false;
    let mut jobs = AnalysisJobs::new();
    loop {
        publish_due_diagnostics(connection, workspace)?;
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
                publish_due_diagnostics(connection, workspace)?;
                continue;
            }
            Err(RecvTimeoutError::Disconnected) => {
                jobs.shutdown();
                return Ok(true);
            }
        };

        match message {
            Message::Request(request) if request.method == "shutdown" => {
                jobs.shutdown();
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
                    handle_request(connection, workspace, request, client_features, &mut jobs)?;
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
                        jobs.cancel(&id);
                    }
                    continue;
                }
                if let Err(error) = handle_notification(
                    connection,
                    workspace,
                    notification,
                    workspace_folders_supported,
                ) {
                    eprintln!("pascal-lsp: notification handling failed: {error}");
                } else if let Some(registration) = watcher_registration.as_mut() {
                    sync_file_watcher(connection, workspace, registration)?;
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
    if let Err(error) = jobs.start(id.clone(), request, workspace, features) {
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
            let locations = workspace.navigate(
                &params.text_document_position_params.text_document.uri,
                params.text_document_position_params.position,
                target,
            );
            send_ok(connection, id, GotoDefinitionResponse::Array(locations))?;
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
            match workspace.formatting_edit(&params.text_document.uri) {
                Ok(Some(edit)) => send_ok(connection, id, vec![edit])?,
                Ok(None) => send_ok(connection, id, Vec::<lsp_types::TextEdit>::new())?,
                Err(error) => send_error(
                    connection,
                    id,
                    ErrorCode::RequestFailed,
                    format!("formatting failed: {error}"),
                )?,
            }
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
) -> Result<(), String> {
    match notification.method.as_str() {
        "initialized" => Ok(()),
        "textDocument/didOpen" => {
            let params: DidOpenTextDocumentParams = parse_notification(&notification)?;
            workspace
                .open_document(
                    params.text_document.uri,
                    params.text_document.text,
                    params.text_document.version,
                )
                .map_err(|error| {
                    eprintln!("pascal-lsp: didOpen ignored: {error}");
                    error
                })
        }
        "textDocument/didChange" => {
            let params: DidChangeTextDocumentParams = parse_notification(&notification)?;
            if params.content_changes.len() != 1 {
                return Err("pascal-lsp only accepts one full-document change".to_string());
            }
            let change = params
                .content_changes
                .into_iter()
                .next()
                .expect("content change length checked");
            if change.range.is_some() {
                return Err("pascal-lsp requires full-document text changes".to_string());
            }
            workspace
                .change_document(
                    params.text_document.uri,
                    change.text,
                    params.text_document.version,
                )
                .map_err(|error| {
                    eprintln!("pascal-lsp: didChange ignored: {error}");
                    error
                })
        }
        "textDocument/didSave" => {
            let params: DidSaveTextDocumentParams = parse_notification(&notification)?;
            workspace
                .save_document(&params.text_document.uri, params.text)
                .map_err(|error| {
                    eprintln!("pascal-lsp: didSave ignored: {error}");
                    error
                })
        }
        "textDocument/didClose" => {
            let params: DidCloseTextDocumentParams = parse_notification(&notification)?;
            if workspace.close_document(&params.text_document.uri) {
                send_diagnostics(connection, &params.text_document.uri, None, Vec::new())
                    .map_err(|error| error.to_string())?;
            }
            Ok(())
        }
        "workspace/didChangeWatchedFiles" => {
            let params: DidChangeWatchedFilesParams = parse_notification(&notification)?;
            for change in params.changes {
                let kind = if change.typ == FileChangeType::CREATED {
                    FileChange::Created
                } else if change.typ == FileChangeType::CHANGED {
                    FileChange::Changed
                } else {
                    FileChange::Deleted
                };
                workspace.file_event(&change.uri, kind);
            }
            Ok(())
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
            Ok(())
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
) -> Result<(), Box<dyn Error + Send + Sync>> {
    for (uri, version, diagnostics) in workspace.take_due_diagnostics() {
        send_diagnostics(connection, &uri, version, diagnostics)?;
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
            "change": 1,
            "save": true
        },
        "declarationProvider": true,
        "definitionProvider": true,
        "implementationProvider": true,
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
    ClientFeatures {
        action_resolve,
        action_disabled,
        document_changes,
    }
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
        AnalysisResult, AnalysisResultValue, BoundedReader, FileWatcherRegistration,
        MAX_CONFIGURATION_WATCH_PATHS, MAX_PAYLOAD_BYTES, MAX_WATCHER_REGISTRATION_RETRIES,
        deliver_analysis_result,
    };
    use crate::workspace::Workspace;
    use lsp_server::{Connection, Message, RequestId, Response};
    use lsp_types::{Position, PrepareRenameResponse, Range, Url};
    use std::fs;
    use std::io::{Cursor, ErrorKind};
    use std::path::PathBuf;

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
        let mut workspace = Workspace::new(vec![root.clone()], Default::default());
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
        let mut workspace = Workspace::new(vec![root], Default::default());
        workspace
            .select_project(&main_uri, Some(&project_a_uri))
            .expect("select project A");
        let source_generation = workspace.source_generation();
        let configuration_generation = workspace.configuration_generation();
        let control_id = RequestId::from("generation-boundary-control".to_string());
        deliver_analysis_result(
            &server,
            &workspace,
            AnalysisResult {
                id: control_id.clone(),
                source_generation,
                configuration_generation,
                records: Vec::new(),
                value: AnalysisResultValue::Prepare(Ok(PrepareRenameResponse::Range(Range::new(
                    Position::new(0, 0),
                    Position::new(0, 1),
                )))),
            },
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
            &workspace,
            AnalysisResult {
                id: id.clone(),
                source_generation: stale_source_generation,
                configuration_generation: stale_configuration_generation,
                records: Vec::new(),
                value: AnalysisResultValue::Prepare(Ok(PrepareRenameResponse::Range(Range::new(
                    Position::new(0, 0),
                    Position::new(0, 1),
                )))),
            },
        )
        .expect("stale result response");

        let Message::Response(response) = client.receiver.recv().expect("delivery response") else {
            panic!("expected a response");
        };
        assert_eq!(response.id, id);
        assert_eq!(response.error.expect("stale result error").code, -32803);
    }
}
