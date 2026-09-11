//! Synchronous stdio LSP protocol loop for the Pascal navigation workspace.

use crate::NavigationTarget;
use crate::workspace::codeactions::{self, ClientActionFeatures};
use crate::workspace::rename::{self, SourceRecord};
use crate::workspace::{FileChange, Workspace, WorkspaceOptions, canonical_file_uri};
use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, bounded, unbounded};
use lsp_server::{Connection, ErrorCode, Message, Notification, Request, RequestId, Response};
use lsp_types::{
    ClientCapabilities, CodeAction, CodeActionOrCommand, CodeActionParams,
    DidChangeTextDocumentParams, DidChangeWatchedFilesParams, DidCloseTextDocumentParams,
    DidOpenTextDocumentParams, DidSaveTextDocumentParams, DocumentFormattingParams, FileChangeType,
    FileSystemWatcher, GlobPattern, GotoDefinitionParams, GotoDefinitionResponse, InitializeParams,
    Position, PrepareRenameResponse, PublishDiagnosticsParams, Registration, RegistrationParams,
    ServerInfo, TextDocumentIdentifier, Url, WatchKind, WorkspaceEdit, WorkspaceFolder,
};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::error::Error;
use std::io::{self, BufRead, Read};
use std::path::PathBuf;
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
    if watcher_registration_supported {
        register_file_watcher(connection)?;
    }

    event_loop(
        connection,
        &mut workspace,
        workspace_folders_supported,
        client_features,
    )
}

fn event_loop(
    connection: &Connection,
    workspace: &mut Workspace,
    workspace_folders_supported: bool,
    client_features: ClientFeatures,
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
                }
            }
            Message::Response(response) => {
                if let Some(error) = response.error {
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

fn register_file_watcher(connection: &Connection) -> Result<(), Box<dyn Error + Send + Sync>> {
    let watchers = vec![FileSystemWatcher {
        glob_pattern: GlobPattern::String("**/*.{pas,dpr,dpk,dproj,optset}".to_string()),
        kind: Some(WatchKind::Create | WatchKind::Change | WatchKind::Delete),
    }];
    let registration = Registration {
        id: "pascal-lsp-file-watcher".to_string(),
        method: "workspace/didChangeWatchedFiles".to_string(),
        register_options: Some(serde_json::to_value(
            lsp_types::DidChangeWatchedFilesRegistrationOptions { watchers },
        )?),
    };
    let request = Request::new(
        RequestId::from("pascal-lsp-register-watcher".to_string()),
        "client/registerCapability".to_string(),
        RegistrationParams {
            registrations: vec![registration],
        },
    );
    connection.sender.send(Message::Request(request))?;
    Ok(())
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
    use super::{BoundedReader, MAX_PAYLOAD_BYTES};
    use lsp_server::Message;
    use std::io::{Cursor, ErrorKind};

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
}
