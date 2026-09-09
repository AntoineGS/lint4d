use std::collections::VecDeque;
use std::fs;
use std::io::{self, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use lsp_server::{Message, Notification, Request, RequestId, Response};
use lsp_types::{Position, Url};
use serde_json::{Value, json};
use tempfile::TempDir;

const IO_TIMEOUT: Duration = Duration::from_secs(5);

struct TestServer {
    child: Child,
    stdin: Option<ChildStdin>,
    messages: Receiver<io::Result<Option<Message>>>,
    pending: VecDeque<Message>,
}

impl TestServer {
    fn launch() -> Self {
        let executable = env!("CARGO_BIN_EXE_pascal-lsp");
        let mut child = Command::new(executable)
            .arg("--stdio")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("launch pascal-lsp");
        let stdout = child.stdout.take().expect("child stdout");
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            loop {
                match Message::read(&mut reader) {
                    Ok(message) => {
                        let is_eof = message.is_none();
                        if sender.send(Ok(message)).is_err() || is_eof {
                            break;
                        }
                    }
                    Err(error) => {
                        let _ = sender.send(Err(error));
                        break;
                    }
                }
            }
        });
        Self {
            stdin: Some(child.stdin.take().expect("child stdin")),
            child,
            messages: receiver,
            pending: VecDeque::new(),
        }
    }

    fn send_request(&mut self, id: impl Into<RequestId>, method: &str, params: Value) {
        self.send(Message::Request(Request::new(
            id.into(),
            method.to_string(),
            params,
        )));
    }

    fn send_notification(&mut self, method: &str, params: Value) {
        self.send(Message::Notification(Notification::new(
            method.to_string(),
            params,
        )));
    }

    fn send(&mut self, message: Message) {
        message
            .write(self.stdin.as_mut().expect("server stdin"))
            .expect("write LSP message");
    }

    fn response(&mut self, expected_id: &RequestId) -> Response {
        if let Some(index) = self.pending.iter().position(
            |message| matches!(message, Message::Response(response) if &response.id == expected_id),
        ) {
            return match self.pending.remove(index).expect("pending response") {
                Message::Response(response) => response,
                _ => unreachable!("pending response predicate"),
            };
        }
        let deadline = Instant::now() + IO_TIMEOUT;
        loop {
            let message = self.receive_until(deadline);
            match message {
                Message::Response(response) if &response.id == expected_id => return response,
                other => self.pending.push_back(other),
            }
        }
    }

    fn notification(&mut self, method: &str) -> Value {
        if let Some(index) = self.pending.iter().position(|message| {
            matches!(message, Message::Notification(notification) if notification.method == method)
        }) {
            return match self.pending.remove(index).expect("pending notification") {
                Message::Notification(notification) => notification.params,
                _ => unreachable!("pending notification predicate"),
            };
        }
        let deadline = Instant::now() + IO_TIMEOUT;
        loop {
            let message = self.receive_until(deadline);
            match message {
                Message::Notification(notification) if notification.method == method => {
                    return notification.params;
                }
                other => self.pending.push_back(other),
            }
        }
    }

    fn request(&mut self, method: &str) -> Request {
        if let Some(index) = self.pending.iter().position(
            |message| matches!(message, Message::Request(request) if request.method == method),
        ) {
            return match self.pending.remove(index).expect("pending request") {
                Message::Request(request) => request,
                _ => unreachable!("pending request predicate"),
            };
        }
        let deadline = Instant::now() + IO_TIMEOUT;
        loop {
            let message = self.receive_until(deadline);
            match message {
                Message::Request(request) if request.method == method => return request,
                other => self.pending.push_back(other),
            }
        }
    }

    fn receive_until(&mut self, deadline: Instant) -> Message {
        let remaining = deadline.saturating_duration_since(Instant::now());
        self.messages
            .recv_timeout(remaining)
            .expect("receive LSP message before timeout")
            .expect("read LSP message")
            .expect("LSP server closed unexpectedly")
    }

    fn initialize(&mut self, root: &Path, initialization_options: Value) -> Value {
        self.initialize_with_watched_registration(root, initialization_options, false)
    }

    fn initialize_with_watched_registration(
        &mut self,
        root: &Path,
        initialization_options: Value,
        dynamic_watched_registration: bool,
    ) -> Value {
        let root_uri = Url::from_file_path(root).expect("workspace URI");
        let id = RequestId::from("initialize".to_string());
        self.send_request(
            id.clone(),
            "initialize",
            json!({
                "processId": null,
                "rootUri": root_uri,
                "initializationOptions": initialization_options,
                "capabilities": {
                    "general": {"positionEncodings": ["utf-16"]},
                    "textDocument": {
                        "synchronization": {"dynamicRegistration": false, "didSave": true},
                        "formatting": {"dynamicRegistration": false},
                        "declaration": {"dynamicRegistration": false},
                        "definition": {"dynamicRegistration": false},
                        "implementation": {"dynamicRegistration": false}
                    },
                    "workspace": {
                        "workspaceFolders": true,
                        "didChangeWatchedFiles": {"dynamicRegistration": dynamic_watched_registration}
                    }
                }
            }),
        );
        let response = self.response(&id);
        assert!(response.error.is_none(), "initialize failed: {response:?}");
        self.send_notification("initialized", json!({}));
        response.result.expect("initialize result")
    }

    fn shutdown(&mut self) {
        let id = RequestId::from("shutdown".to_string());
        self.send_request(id.clone(), "shutdown", Value::Null);
        let response = self.response(&id);
        assert!(response.error.is_none(), "shutdown failed: {response:?}");
        self.send_notification("exit", Value::Null);
        self.stdin.take();
        let status = self.child.wait().expect("wait for LSP server");
        assert!(
            status.success(),
            "LSP server exited unsuccessfully: {status}"
        );
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        if self.child.try_wait().expect("poll LSP server").is_none() {
            let _ = self.stdin.take();
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

fn uri(path: &Path) -> Url {
    Url::from_file_path(path).expect("file URI")
}

fn position_of(source: &str, needle: &str, occurrence: usize) -> Position {
    let mut from = 0;
    let mut offset = 0;
    for _ in 0..=occurrence {
        let relative = source[from..]
            .find(needle)
            .unwrap_or_else(|| panic!("missing {needle:?} occurrence {occurrence}"));
        offset = from + relative;
        from = offset + needle.len();
    }
    let line_start = source[..offset].rfind('\n').map_or(0, |index| index + 1);
    Position::new(
        source[..offset]
            .bytes()
            .filter(|byte| *byte == b'\n')
            .count() as u32,
        source[line_start..offset].encode_utf16().count() as u32,
    )
}

fn navigation_params(path: &Path, source: &str, needle: &str, occurrence: usize) -> Value {
    json!({
        "textDocument": {"uri": uri(path)},
        "position": position_of(source, needle, occurrence),
    })
}

fn result_locations(response: Response) -> Vec<Value> {
    assert!(response.error.is_none(), "request failed: {response:?}");
    response
        .result
        .expect("request result")
        .as_array()
        .expect("array location result")
        .clone()
}

fn write_file(path: &Path, source: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create source directory");
    }
    fs::write(path, source).expect("write Pascal source");
}

fn standard_workspace() -> (TempDir, PathBuf, PathBuf, String, String) {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path().join("workspace with spaces");
    let provider = root.join("Provider.pas");
    let main = root.join("Main.pas");
    let provider_source = "unit Provider;\ninterface\nprocedure PublicRoutine;\nimplementation\nprocedure PublicRoutine;\nbegin\nend;\nend.\n".to_string();
    let main_source = "unit Main;\ninterface\nuses Provider;\nimplementation\nprocedure Run;\nbegin\n  PublicRoutine;\nend;\nend.\n".to_string();
    write_file(&provider, &provider_source);
    write_file(&main, &main_source);
    (temp, main, provider, main_source, provider_source)
}

#[test]
fn initialize_advertises_utf16_sync_navigation_and_formatting() {
    let (_temp, main, _provider, _main_source, _provider_source) = standard_workspace();
    let root = main.parent().expect("workspace root");
    let mut server = TestServer::launch();
    let result = server.initialize(root, Value::Null);
    let capabilities = &result["capabilities"];
    assert_eq!(capabilities["positionEncoding"], "utf-16");
    assert_eq!(capabilities["textDocumentSync"]["change"], 1);
    assert_eq!(capabilities["declarationProvider"], true);
    assert_eq!(capabilities["definitionProvider"], true);
    assert_eq!(capabilities["implementationProvider"], true);
    assert_eq!(capabilities["documentFormattingProvider"], true);
    server.shutdown();
}

#[test]
fn dynamic_watcher_registration_is_conditional_and_acknowledged() {
    let (_temp, main, _provider, _main_source, _provider_source) = standard_workspace();
    let root = main.parent().expect("workspace root");
    let mut server = TestServer::launch();
    server.initialize_with_watched_registration(root, Value::Null, true);
    let registration = server.request("client/registerCapability");
    assert_eq!(
        registration.params["registrations"][0]["method"],
        "workspace/didChangeWatchedFiles"
    );
    server.send(Message::Response(Response::new_ok(
        registration.id,
        Value::Null,
    )));
    server.shutdown();
}

#[test]
fn invalid_initialize_can_be_retried_without_hanging_the_transport() {
    let (_temp, main, _provider, _main_source, _provider_source) = standard_workspace();
    let root = main.parent().expect("workspace root");
    let mut server = TestServer::launch();
    let invalid_id = RequestId::from("invalid-initialize".to_string());
    server.send_request(
        invalid_id.clone(),
        "initialize",
        json!({"processId": null, "rootUri": uri(root), "capabilities": "invalid"}),
    );
    let invalid = server.response(&invalid_id);
    assert_eq!(
        invalid.error.expect("invalid initialize error").code,
        -32602
    );

    let result = server.initialize(root, Value::Null);
    assert!(result["capabilities"].is_object());
    server.shutdown();
}

#[test]
fn real_process_navigates_declaration_definition_and_implementation_across_units() {
    let (_temp, main, provider, main_source, _provider_source) = standard_workspace();
    let root = main.parent().expect("workspace root");
    let mut server = TestServer::launch();
    server.initialize(root, Value::Null);

    for (id, method, expected_line) in [
        ("decl", "textDocument/declaration", 2),
        ("def", "textDocument/definition", 4),
        ("impl", "textDocument/implementation", 4),
    ] {
        let request_id = RequestId::from(id.to_string());
        server.send_request(
            request_id.clone(),
            method,
            navigation_params(&main, &main_source, "PublicRoutine", 0),
        );
        let locations = result_locations(server.response(&request_id));
        assert_eq!(locations.len(), 1);
        assert_eq!(locations[0]["uri"], uri(&provider).to_string());
        assert_eq!(locations[0]["range"]["start"]["line"], expected_line);
    }
    server.shutdown();
}

#[test]
fn real_process_navigates_typed_property_to_read_accessor() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path().join("property workspace");
    let provider = root.join("Provider.pas");
    let main = root.join("Main.pas");
    let provider_source = "unit Provider;\ninterface\ntype\n  TConfig = class\n  private\n    FValue: string;\n    procedure SetValue(const Value: string);\n  public\n    property Value: string read FValue write SetValue;\n  end;\nimplementation\nprocedure TConfig.SetValue(const Value: string);\nbegin\n  FValue := Value;\nend;\nend.\n";
    let main_source = "unit Main;\ninterface\nuses Provider;\nimplementation\nprocedure Run;\nvar\n  Config: TConfig;\nbegin\n  Config.Value := '|';\nend;\nend.\n";
    write_file(&provider, provider_source);
    write_file(&main, main_source);

    let root = main.parent().expect("workspace root");
    let mut server = TestServer::launch();
    server.initialize(root, Value::Null);

    for (id, method, expected_line) in [
        ("property-declaration", "textDocument/declaration", 8),
        ("property-definition", "textDocument/definition", 5),
        ("property-implementation", "textDocument/implementation", 5),
    ] {
        let request_id = RequestId::from(id.to_string());
        server.send_request(
            request_id.clone(),
            method,
            navigation_params(&main, main_source, "Value", 0),
        );
        let locations = result_locations(server.response(&request_id));
        assert_eq!(locations.len(), 1);
        assert_eq!(locations[0]["uri"], uri(&provider).to_string());
        assert_eq!(locations[0]["range"]["start"]["line"], expected_line);
    }
    server.shutdown();
}

#[test]
fn unsaved_unicode_crlf_overlays_win_and_close_restores_disk() {
    let (_temp, main, provider, main_source, provider_source) = standard_workspace();
    let root = main.parent().expect("workspace root");
    let mut server = TestServer::launch();
    server.initialize(root, Value::Null);

    let overlay_provider = provider_source
        .replace("PublicRoutine", "NewRoutine")
        .replace('\n', "\r\n");
    let overlay_main = main_source
        .replace("unit Main;", "unit Main;\n// 😀 Unicode")
        .replace("PublicRoutine", "NewRoutine")
        .replace('\n', "\r\n");
    server.send_notification(
        "textDocument/didOpen",
        json!({"textDocument": {"uri": uri(&provider), "languageId": "pascal", "version": 1, "text": overlay_provider}}),
    );
    server.send_notification(
        "textDocument/didOpen",
        json!({"textDocument": {"uri": uri(&main), "languageId": "pascal", "version": 1, "text": overlay_main.clone()}}),
    );
    let _ = server.notification("textDocument/publishDiagnostics");
    let _ = server.notification("textDocument/publishDiagnostics");

    let request_id = RequestId::from("overlay".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/declaration",
        navigation_params(&main, &overlay_main, "NewRoutine", 0),
    );
    assert_eq!(result_locations(server.response(&request_id)).len(), 1);

    let changed_provider = overlay_provider.replace("NewRoutine", "ChangedRoutine");
    let changed_main = overlay_main.replace("NewRoutine", "ChangedRoutine");
    server.send_notification(
        "textDocument/didChange",
        json!({"textDocument": {"uri": uri(&provider), "version": 2}, "contentChanges": [{"text": changed_provider}]}),
    );
    server.send_notification(
        "textDocument/didChange",
        json!({"textDocument": {"uri": uri(&main), "version": 2}, "contentChanges": [{"text": changed_main.clone()}]}),
    );
    let _ = server.notification("textDocument/publishDiagnostics");
    let _ = server.notification("textDocument/publishDiagnostics");
    let request_id = RequestId::from("changed".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/declaration",
        navigation_params(&main, &changed_main, "ChangedRoutine", 0),
    );
    assert_eq!(result_locations(server.response(&request_id)).len(), 1);

    server.send_notification(
        "textDocument/didClose",
        json!({"textDocument": {"uri": uri(&provider)}}),
    );
    let cleared = server.notification("textDocument/publishDiagnostics");
    assert_eq!(cleared["uri"], uri(&provider).to_string());
    assert!(
        cleared["diagnostics"]
            .as_array()
            .expect("diagnostics array")
            .is_empty()
    );

    let request_id = RequestId::from("stale".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/declaration",
        navigation_params(&main, &overlay_main, "NewRoutine", 0),
    );
    assert!(result_locations(server.response(&request_id)).is_empty());

    let old_main = overlay_main.replace("NewRoutine", "PublicRoutine");
    server.send_notification(
        "textDocument/didChange",
        json!({"textDocument": {"uri": uri(&main), "version": 3}, "contentChanges": [{"text": old_main.clone()}]}),
    );
    let _ = server.notification("textDocument/publishDiagnostics");
    let request_id = RequestId::from("before-delete".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/declaration",
        navigation_params(&main, &old_main, "PublicRoutine", 0),
    );
    assert_eq!(result_locations(server.response(&request_id)).len(), 1);

    server.send_notification(
        "workspace/didChangeWatchedFiles",
        json!({"changes": [{"uri": uri(&provider), "type": 3}]}),
    );
    let request_id = RequestId::from("after-delete".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/declaration",
        navigation_params(&main, &old_main, "PublicRoutine", 0),
    );
    assert!(result_locations(server.response(&request_id)).is_empty());

    write_file(&provider, &provider_source);
    server.send_notification(
        "workspace/didChangeWatchedFiles",
        json!({"changes": [{"uri": uri(&provider), "type": 1}]}),
    );
    let request_id = RequestId::from("after-create".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/declaration",
        navigation_params(&main, &old_main, "PublicRoutine", 0),
    );
    assert_eq!(result_locations(server.response(&request_id)).len(), 1);
    server.shutdown();
}

#[test]
fn rejected_overlay_clears_stale_state_and_recovers_on_newer_version() {
    let (_temp, main, provider, main_source, provider_source) = standard_workspace();
    let root = main.parent().expect("workspace root");
    let overlay_provider = provider_source.replace("PublicRoutine", "NewRoutine");
    let overlay_main = main_source.replace("PublicRoutine", "NewRoutine");
    let mut server = TestServer::launch();
    server.initialize(root, json!({"maxFileBytes": 256}));

    server.send_notification(
        "textDocument/didOpen",
        json!({"textDocument": {"uri": uri(&provider), "languageId": "pascal", "version": 1, "text": overlay_provider}}),
    );
    server.send_notification(
        "textDocument/didOpen",
        json!({"textDocument": {"uri": uri(&main), "languageId": "pascal", "version": 1, "text": overlay_main.clone()}}),
    );
    let _ = server.notification("textDocument/publishDiagnostics");
    let _ = server.notification("textDocument/publishDiagnostics");

    let request_id = RequestId::from("before-rejection".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/declaration",
        navigation_params(&main, &overlay_main, "NewRoutine", 0),
    );
    assert_eq!(result_locations(server.response(&request_id)).len(), 1);

    let oversized = "x".repeat(300);
    server.send_notification(
        "textDocument/didChange",
        json!({"textDocument": {"uri": uri(&provider), "version": 2}, "contentChanges": [{"text": oversized}]}),
    );
    let rejected = server.notification("textDocument/publishDiagnostics");
    assert_eq!(rejected["uri"], uri(&provider).to_string());
    assert!(
        rejected["diagnostics"][0]["message"]
            .as_str()
            .expect("rejection diagnostic")
            .contains("limit")
    );

    let format_id = RequestId::from("rejected-format".to_string());
    server.send_request(
        format_id.clone(),
        "textDocument/formatting",
        json!({"textDocument": {"uri": uri(&provider)}, "options": {"tabSize": 2, "insertSpaces": true}}),
    );
    let format_response = server.response(&format_id);
    assert!(
        format_response
            .error
            .expect("rejected formatting error")
            .message
            .contains("rejected")
    );

    let request_id = RequestId::from("after-rejection".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/declaration",
        navigation_params(&main, &overlay_main, "NewRoutine", 0),
    );
    assert!(result_locations(server.response(&request_id)).is_empty());

    let restored_provider = provider_source.replace("PublicRoutine", "RestoredRoutine");
    let restored_main = main_source.replace("PublicRoutine", "RestoredRoutine");
    server.send_notification(
        "textDocument/didChange",
        json!({"textDocument": {"uri": uri(&provider), "version": 3}, "contentChanges": [{"text": restored_provider}]}),
    );
    server.send_notification(
        "textDocument/didChange",
        json!({"textDocument": {"uri": uri(&main), "version": 2}, "contentChanges": [{"text": restored_main.clone()}]}),
    );
    let _ = server.notification("textDocument/publishDiagnostics");
    let _ = server.notification("textDocument/publishDiagnostics");
    let request_id = RequestId::from("after-recovery".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/declaration",
        navigation_params(&main, &restored_main, "RestoredRoutine", 0),
    );
    assert_eq!(result_locations(server.response(&request_id)).len(), 1);
    server.shutdown();
}

#[test]
fn rejected_open_buffers_respect_file_and_total_budgets_without_disk_fallback() {
    let source = "unit Buffer;\ninterface\nprocedure Run;\nimplementation\nprocedure Run; begin end;\nend.\n";

    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path().join("file-budget");
    let first = root.join("First.pas");
    let second = root.join("Second.pas");
    let mut server = TestServer::launch();
    server.initialize(&root, json!({"maxFiles": 1, "maxTotalBytes": 1024}));
    thread::sleep(Duration::from_millis(50));
    write_file(&first, source);
    write_file(&second, source);
    server.send_notification(
        "textDocument/didOpen",
        json!({"textDocument": {"uri": uri(&first), "languageId": "pascal", "version": 1, "text": source}}),
    );
    let _ = server.notification("textDocument/publishDiagnostics");
    server.send_notification(
        "textDocument/didOpen",
        json!({"textDocument": {"uri": uri(&second), "languageId": "pascal", "version": 1, "text": source}}),
    );
    let file_rejected = server.notification("textDocument/publishDiagnostics");
    assert!(
        file_rejected["diagnostics"][0]["message"]
            .as_str()
            .expect("file budget diagnostic")
            .contains("file limit")
    );
    let format_id = RequestId::from("file-budget-format".to_string());
    server.send_request(
        format_id.clone(),
        "textDocument/formatting",
        json!({"textDocument": {"uri": uri(&second)}, "options": {"tabSize": 2, "insertSpaces": true}}),
    );
    assert!(server.response(&format_id).error.is_some());
    server.shutdown();

    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path().join("total-budget");
    let first = root.join("First.pas");
    let second = root.join("Second.pas");
    let mut server = TestServer::launch();
    server.initialize(
        &root,
        json!({"maxFiles": 4, "maxTotalBytes": source.len() * 2 + 1}),
    );
    thread::sleep(Duration::from_millis(50));
    write_file(&first, source);
    write_file(&second, source);
    server.send_notification(
        "textDocument/didOpen",
        json!({"textDocument": {"uri": uri(&first), "languageId": "pascal", "version": 1, "text": source}}),
    );
    let _ = server.notification("textDocument/publishDiagnostics");
    server.send_notification(
        "textDocument/didOpen",
        json!({"textDocument": {"uri": uri(&second), "languageId": "pascal", "version": 1, "text": source}}),
    );
    let total_rejected = server.notification("textDocument/publishDiagnostics");
    assert!(
        total_rejected["diagnostics"][0]["message"]
            .as_str()
            .expect("total budget diagnostic")
            .contains("total source limit")
    );
    let format_id = RequestId::from("total-budget-format".to_string());
    server.send_request(
        format_id.clone(),
        "textDocument/formatting",
        json!({"textDocument": {"uri": uri(&second)}, "options": {"tabSize": 2, "insertSpaces": true}}),
    );
    assert!(server.response(&format_id).error.is_some());
    server.shutdown();
}

#[test]
fn source_paths_and_exclusions_control_cross_file_indexing() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path().join("workspace");
    let src = root.join("src");
    let extra = root.join("extra path");
    let excluded = src.join("excluded");
    let main = src.join("Main.pas");
    let extra_provider = extra.join("Extra.pas");
    let excluded_provider = excluded.join("Hidden.pas");
    let main_source = "unit Main;\ninterface\nuses Extra, Hidden;\nimplementation\nprocedure Run;\nbegin\n  ExtraRoutine;\n  HiddenRoutine;\nend;\nend.\n";
    write_file(&main, main_source);
    write_file(
        &extra_provider,
        "unit Extra;\ninterface\nprocedure ExtraRoutine;\nimplementation\nprocedure ExtraRoutine; begin end;\nend.\n",
    );
    write_file(
        &excluded_provider,
        "unit Hidden;\ninterface\nprocedure HiddenRoutine;\nimplementation\nprocedure HiddenRoutine; begin end;\nend.\n",
    );

    let mut server = TestServer::launch();
    server.initialize(
        &root,
        json!({"sourcePaths": ["src", extra], "exclude": ["**/excluded/**"]}),
    );

    for (id, method, needle, expected) in [
        (
            "extra",
            "textDocument/declaration",
            "ExtraRoutine",
            extra_provider.clone(),
        ),
        (
            "hidden",
            "textDocument/declaration",
            "HiddenRoutine",
            excluded_provider.clone(),
        ),
    ] {
        let request_id = RequestId::from(id.to_string());
        server.send_request(
            request_id.clone(),
            method,
            navigation_params(&main, main_source, needle, 0),
        );
        let locations = result_locations(server.response(&request_id));
        if needle == "ExtraRoutine" {
            assert_eq!(locations.len(), 1);
            assert_eq!(locations[0]["uri"], uri(&expected).to_string());
        } else {
            assert!(locations.is_empty());
        }
    }
    server.shutdown();
}

#[test]
fn explicit_roots_allow_excluded_ancestor_names_and_add_source_paths() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path().join(".worktrees").join("project");
    let extra = root.join("build");
    let main = root.join("Main.pas");
    let provider = extra.join("Provider.pas");
    let main_source = "unit Main;\ninterface\nuses Provider;\nimplementation\nprocedure Run;\nbegin\n  BuildRoutine;\nend;\nend.\n";
    write_file(&main, main_source);
    write_file(
        &provider,
        "unit Provider;\ninterface\nprocedure BuildRoutine;\nimplementation\nprocedure BuildRoutine; begin end;\nend.\n",
    );

    let mut server = TestServer::launch();
    server.initialize(&root, json!({"sourcePaths": ["./build/../build"]}));

    let request_id = RequestId::from("root-and-extra-source".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/declaration",
        navigation_params(&main, main_source, "BuildRoutine", 0),
    );
    let locations = result_locations(server.response(&request_id));
    assert_eq!(locations.len(), 1);
    assert_eq!(locations[0]["uri"], uri(&provider).to_string());
    server.shutdown();
}

#[test]
fn navigation_revalidates_disk_without_watcher_notifications() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path().join("workspace");
    let external = temp.path().join("external sources");
    let main = root.join("Main.pas");
    let provider = root.join("Provider.pas");
    let external_provider = external.join("External.PAS");
    let main_source = "unit Main;\ninterface\nuses Provider, External;\nimplementation\nprocedure Run;\nbegin\n  PublicRoutine;\n  ExternalRoutine;\nend;\nend.\n";
    let provider_source = "unit Provider;\ninterface\nprocedure PublicRoutine;\nimplementation\nprocedure PublicRoutine; begin end;\nend.\n";
    write_file(&main, main_source);
    write_file(&provider, provider_source);

    let mut server = TestServer::launch();
    server.initialize(&root, json!({"sourcePaths": [external.clone()]}));

    let request_id = RequestId::from("initial-disk".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/declaration",
        navigation_params(&main, main_source, "PublicRoutine", 0),
    );
    assert_eq!(result_locations(server.response(&request_id)).len(), 1);

    let changed_provider = provider_source.replace("PublicRoutine", "ChangedRoutine");
    let changed_main = main_source.replace("PublicRoutine", "ChangedRoutine");
    write_file(&provider, &changed_provider);
    write_file(&main, &changed_main);
    let request_id = RequestId::from("changed-disk".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/declaration",
        navigation_params(&main, &changed_main, "ChangedRoutine", 0),
    );
    assert_eq!(result_locations(server.response(&request_id)).len(), 1);

    let overlay_provider = changed_provider.replace("ChangedRoutine", "OverlayRoutine");
    let overlay_main = changed_main.replace("ChangedRoutine", "OverlayRoutine");
    server.send_notification(
        "textDocument/didOpen",
        json!({"textDocument": {"uri": uri(&provider), "languageId": "pascal", "version": 1, "text": overlay_provider}}),
    );
    server.send_notification(
        "textDocument/didOpen",
        json!({"textDocument": {"uri": uri(&main), "languageId": "pascal", "version": 1, "text": overlay_main.clone()}}),
    );
    let _ = server.notification("textDocument/publishDiagnostics");
    let _ = server.notification("textDocument/publishDiagnostics");
    write_file(
        &provider,
        &changed_provider.replace("ChangedRoutine", "DiskRoutine"),
    );
    write_file(
        &main,
        &changed_main.replace("ChangedRoutine", "DiskRoutine"),
    );
    let request_id = RequestId::from("overlay-wins".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/declaration",
        navigation_params(&main, &overlay_main, "OverlayRoutine", 0),
    );
    assert_eq!(result_locations(server.response(&request_id)).len(), 1);
    server.send_notification(
        "textDocument/didClose",
        json!({"textDocument": {"uri": uri(&provider)}}),
    );
    server.send_notification(
        "textDocument/didClose",
        json!({"textDocument": {"uri": uri(&main)}}),
    );
    let _ = server.notification("textDocument/publishDiagnostics");
    let _ = server.notification("textDocument/publishDiagnostics");

    fs::remove_file(&provider).expect("delete provider");
    let request_id = RequestId::from("deleted-disk".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/declaration",
        navigation_params(
            &main,
            &changed_main.replace("ChangedRoutine", "DiskRoutine"),
            "DiskRoutine",
            0,
        ),
    );
    assert!(result_locations(server.response(&request_id)).is_empty());

    write_file(
        &external_provider,
        "unit External;\ninterface\nprocedure ExternalRoutine;\nimplementation\nprocedure ExternalRoutine; begin end;\nend.\n",
    );
    thread::sleep(Duration::from_millis(350));
    let request_id = RequestId::from("new-external-disk".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/declaration",
        navigation_params(&main, main_source, "ExternalRoutine", 0),
    );
    let locations = result_locations(server.response(&request_id));
    assert_eq!(locations.len(), 1);
    assert_eq!(locations[0]["uri"], uri(&external_provider).to_string());
    server.shutdown();
}

#[test]
fn diagnostics_use_utf16_columns_and_formatting_is_in_memory() {
    let (_temp, main, _provider, _main_source, _provider_source) = standard_workspace();
    let root = main.parent().expect("workspace root");
    let original = "unit Main;\r\ninterface\r\nimplementation\r\nprocedure Run;\r\nbegin\r\n  S := '😀'; with Obj do begin end;\r\nend.\r\n";
    write_file(&main, original);
    let mut server = TestServer::launch();
    server.initialize(root, Value::Null);
    server.send_notification(
        "textDocument/didOpen",
        json!({"textDocument": {"uri": uri(&main), "languageId": "pascal", "version": 1, "text": original}}),
    );
    let diagnostics = server.notification("textDocument/publishDiagnostics");
    let diagnostic = diagnostics["diagnostics"]
        .as_array()
        .expect("diagnostics array")
        .iter()
        .find(|diagnostic| diagnostic["code"] == "with-statement")
        .expect("parse diagnostic");
    assert_eq!(diagnostic["range"]["start"]["line"], 5);
    assert_eq!(diagnostic["range"]["start"]["character"], 13);

    let format_path = root.join("Format.pas");
    let format_source = "unit Format; interface implementation procedure Run; begin end; end.";
    write_file(&format_path, format_source);
    server.send_notification(
        "textDocument/didOpen",
        json!({"textDocument": {"uri": uri(&format_path), "languageId": "pascal", "version": 1, "text": format_source}}),
    );
    let request_id = RequestId::from("format".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/formatting",
        json!({"textDocument": {"uri": uri(&format_path)}, "options": {"tabSize": 2, "insertSpaces": true}}),
    );
    let response = server.response(&request_id);
    assert!(response.error.is_none(), "formatting failed: {response:?}");
    assert!(response.result.expect("formatting result").is_array());
    assert_eq!(
        fs::read_to_string(&format_path).expect("read original"),
        format_source
    );
    server.shutdown();
}

#[test]
fn diagnostics_normalize_bare_carriage_returns_for_lint_positions() {
    let (_temp, main, _provider, _main_source, _provider_source) = standard_workspace();
    let root = main.parent().expect("workspace root");
    let source = "unit Main;\rinterface\rimplementation\rprocedure Run;\rbegin\r  S := '😀'; with Obj do begin end;\rend.\r";
    write_file(&main, source);
    let mut server = TestServer::launch();
    server.initialize(root, Value::Null);
    server.send_notification(
        "textDocument/didOpen",
        json!({"textDocument": {"uri": uri(&main), "languageId": "pascal", "version": 1, "text": source}}),
    );
    let diagnostics = server.notification("textDocument/publishDiagnostics");
    let diagnostic = diagnostics["diagnostics"]
        .as_array()
        .expect("diagnostics array")
        .iter()
        .find(|diagnostic| diagnostic["code"] == "with-statement")
        .expect("with diagnostic");
    assert_eq!(diagnostic["range"]["start"]["line"], 5);
    assert_eq!(diagnostic["range"]["start"]["character"], 13);
    server.shutdown();
}

#[test]
fn formatting_reads_unopened_disk_documents_without_writing_them() {
    let (_temp, main, _provider, _main_source, _provider_source) = standard_workspace();
    let root = main.parent().expect("workspace root");
    let format_path = root.join("UnopenedFormat.pas");
    let format_source =
        "unit UnopenedFormat; interface implementation procedure Run; begin end; end.";
    write_file(&format_path, format_source);

    let mut server = TestServer::launch();
    server.initialize(root, Value::Null);
    let request_id = RequestId::from("unopened-format".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/formatting",
        json!({
            "textDocument": {"uri": uri(&format_path)},
            "options": {"tabSize": 2, "insertSpaces": true}
        }),
    );
    let response = server.response(&request_id);
    assert!(response.error.is_none(), "formatting failed: {response:?}");
    assert!(response.result.expect("formatting result").is_array());
    assert_eq!(
        fs::read_to_string(&format_path).expect("read original"),
        format_source
    );
    server.shutdown();
}

#[test]
fn unopened_formatting_rejects_oversized_and_non_regular_disk_sources() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path().join("workspace");
    let oversized = root.join("Oversized.pas");
    write_file(&oversized, &"x".repeat(64));
    let non_regular = root.join("Directory.pas");
    fs::create_dir_all(&non_regular).expect("create directory with Pascal suffix");

    let mut server = TestServer::launch();
    server.initialize(&root, json!({"maxFileBytes": 32}));
    for (id, path, expected) in [
        ("oversized-format", oversized, "per-file limit"),
        ("non-regular-format", non_regular, "regular file"),
    ] {
        let request_id = RequestId::from(id.to_string());
        server.send_request(
            request_id.clone(),
            "textDocument/formatting",
            json!({"textDocument": {"uri": uri(&path)}, "options": {"tabSize": 2, "insertSpaces": true}}),
        );
        let response = server.response(&request_id);
        assert!(
            response.error.is_some(),
            "expected formatting error: {response:?}"
        );
        assert!(
            response
                .error
                .expect("formatting error")
                .message
                .contains(expected)
        );
    }
    server.shutdown();
}

#[test]
fn invalid_requests_return_standard_errors_and_deep_analysis_is_guarded() {
    let (_temp, main, _provider, _main_source, _provider_source) = standard_workspace();
    let root = main.parent().expect("workspace root");
    let mut server = TestServer::launch();
    server.initialize(root, Value::Null);

    let unknown_id = RequestId::from("unknown".to_string());
    server.send_request(unknown_id.clone(), "pascal/unknown", json!({}));
    let unknown = server.response(&unknown_id);
    assert_eq!(unknown.error.expect("unknown method error").code, -32601);

    let invalid_id = RequestId::from("invalid".to_string());
    server.send_request(invalid_id.clone(), "textDocument/definition", json!({}));
    let invalid = server.response(&invalid_id);
    assert_eq!(invalid.error.expect("invalid params error").code, -32602);

    let mut deep =
        String::from("unit Deep;\ninterface\nimplementation\nprocedure Run;\nbegin\n  X := ");
    deep.push_str(&"(".repeat(400));
    deep.push('1');
    deep.push_str(&")".repeat(400));
    deep.push_str(";\nend;\nend.\n");
    let deep_path = root.join("Deep.pas");
    write_file(&deep_path, &deep);
    server.send_notification(
        "textDocument/didOpen",
        json!({"textDocument": {"uri": uri(&deep_path), "languageId": "pascal", "version": 1, "text": deep}}),
    );
    let deep_diagnostics = server.notification("textDocument/publishDiagnostics");
    assert!(
        deep_diagnostics["diagnostics"]
            .as_array()
            .expect("deep diagnostics")
            .iter()
            .any(|diagnostic| diagnostic["message"]
                .as_str()
                .unwrap_or_default()
                .contains("depth"))
    );

    let format_id = RequestId::from("deep-format".to_string());
    server.send_request(
        format_id.clone(),
        "textDocument/formatting",
        json!({"textDocument": {"uri": uri(&deep_path)}, "options": {"tabSize": 2, "insertSpaces": true}}),
    );
    assert!(server.response(&format_id).error.is_some());
    server.shutdown();
}

#[test]
fn shutdown_and_eof_leave_no_protocol_text_on_stdout() {
    let (_temp, main, _provider, _main_source, _provider_source) = standard_workspace();
    let root = main.parent().expect("workspace root");
    let mut server = TestServer::launch();
    let result = server.initialize(root, json!({"sourcePaths": []}));
    assert!(result["capabilities"].is_object());
    server.stdin.take();
    let status = server.child.wait().expect("wait after EOF");
    assert!(status.success(), "EOF should terminate cleanly: {status}");
}
