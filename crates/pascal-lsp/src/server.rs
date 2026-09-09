//! Synchronous stdio LSP protocol loop for the Pascal navigation workspace.

use crate::NavigationTarget;
use crate::workspace::{FileChange, Workspace, WorkspaceOptions};
use crossbeam_channel::{RecvTimeoutError, bounded};
use lsp_server::{Connection, ErrorCode, Message, Notification, Request, RequestId, Response};
use lsp_types::{
    ClientCapabilities, DeclarationCapability, DidChangeTextDocumentParams,
    DidChangeWatchedFilesParams, DidCloseTextDocumentParams, DidOpenTextDocumentParams,
    DidSaveTextDocumentParams, DocumentFormattingParams, FileChangeType, FileSystemWatcher,
    GlobPattern, GotoDefinitionParams, GotoDefinitionResponse, InitializeParams, OneOf,
    PublishDiagnosticsParams, Registration, RegistrationParams, ServerCapabilities, ServerInfo,
    TextDocumentSyncCapability, TextDocumentSyncKind, TextDocumentSyncOptions,
    TextDocumentSyncSaveOptions, Url, WatchKind, WorkspaceFolder,
    WorkspaceFoldersServerCapabilities,
};
use serde::de::DeserializeOwned;
use std::error::Error;
use std::io::{self, BufRead, Read};
use std::path::PathBuf;
use std::thread::{self, JoinHandle};
use std::time::Duration;

const SERVER_NAME: &str = "pascal-lsp";
const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");
const MAX_PAYLOAD_BYTES: usize = 8 * 1024 * 1024;
const MAX_HEADER_BYTES: usize = 8 * 1024;

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
    // The initialize result is already on the wire before this bounded scan begins.
    workspace.scan();
    if watcher_registration_supported {
        register_file_watcher(connection)?;
    }

    event_loop(connection, &mut workspace, workspace_folders_supported)
}

fn event_loop(
    connection: &Connection,
    workspace: &mut Workspace,
    workspace_folders_supported: bool,
) -> Result<bool, Box<dyn Error + Send + Sync>> {
    let mut shutdown_received = false;
    loop {
        publish_due_diagnostics(connection, workspace)?;
        let timeout = workspace
            .next_diagnostic_timeout()
            .unwrap_or(Duration::from_secs(86_400));
        let message = match connection.receiver.recv_timeout(timeout) {
            Ok(message) => message,
            Err(RecvTimeoutError::Timeout) => {
                publish_due_diagnostics(connection, workspace)?;
                continue;
            }
            Err(RecvTimeoutError::Disconnected) => return Ok(true),
        };

        match message {
            Message::Request(request) if request.method == "shutdown" => {
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
                    handle_request(connection, workspace, request)?;
                }
            }
            Message::Notification(notification) if notification.method == "exit" => {
                return Ok(shutdown_received);
            }
            Message::Notification(notification) => {
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

fn handle_request(
    connection: &Connection,
    workspace: &mut Workspace,
    request: Request,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    match request.method.as_str() {
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
            workspace.refresh_for_navigation();
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
        glob_pattern: GlobPattern::String("**/*.{pas,dpr,dpk}".to_string()),
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

fn server_capabilities(client: &ClientCapabilities) -> ServerCapabilities {
    let workspace =
        supports_workspace_folders(client).then_some(lsp_types::WorkspaceServerCapabilities {
            workspace_folders: Some(WorkspaceFoldersServerCapabilities {
                supported: Some(true),
                change_notifications: Some(OneOf::Left(true)),
            }),
            ..Default::default()
        });
    ServerCapabilities {
        position_encoding: Some(lsp_types::PositionEncodingKind::UTF16),
        text_document_sync: Some(TextDocumentSyncCapability::Options(
            TextDocumentSyncOptions {
                open_close: Some(true),
                change: Some(TextDocumentSyncKind::FULL),
                save: Some(TextDocumentSyncSaveOptions::Supported(true)),
                ..Default::default()
            },
        )),
        declaration_provider: Some(DeclarationCapability::Simple(true)),
        definition_provider: Some(OneOf::Left(true)),
        implementation_provider: Some(lsp_types::ImplementationProviderCapability::Simple(true)),
        document_formatting_provider: Some(OneOf::Left(true)),
        workspace,
        ..Default::default()
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
