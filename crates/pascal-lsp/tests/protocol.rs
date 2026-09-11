use std::collections::{HashSet, VecDeque};
#[cfg(target_os = "linux")]
use std::ffi::CString;
use std::fs;
use std::io::{self, BufReader};
#[cfg(target_os = "linux")]
use std::os::fd::FromRawFd;
#[cfg(target_os = "linux")]
use std::os::unix::fs::MetadataExt;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
#[cfg(unix)]
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use lsp_server::{Message, Notification, Request, RequestId, Response};
use lsp_types::{Position, Url};
use pascal_lsp::workspace::{FileChange, Workspace, WorkspaceOptions};
use pascal_lsp::{NavigationTarget, ProjectContext};
use serde_json::{Value, json};
use tempfile::TempDir;

#[cfg(target_os = "linux")]
unsafe extern "C" {
    fn inotify_init1(flags: i32) -> i32;
    fn inotify_add_watch(fd: i32, pathname: *const std::os::raw::c_char, mask: u32) -> i32;
    fn utimensat(
        dirfd: i32,
        pathname: *const std::os::raw::c_char,
        times: *const Timespec,
        flags: i32,
    ) -> i32;
}

#[cfg(target_os = "linux")]
const IN_CLOSE_NOWRITE: u32 = 0x0000_0010;

#[cfg(target_os = "linux")]
const IN_OPEN: u32 = 0x0000_0020;

#[cfg(target_os = "linux")]
const AT_FDCWD: i32 = -100;

#[cfg(target_os = "linux")]
const UTIME_OMIT: i64 = 1_073_741_822;

#[cfg(target_os = "linux")]
#[repr(C)]
struct Timespec {
    tv_sec: i64,
    tv_nsec: i64,
}

#[cfg(target_os = "linux")]
fn wait_for_close_events(fd: i32, expected: usize) {
    let mut file = unsafe { std::fs::File::from_raw_fd(fd) };
    let mut events = [0_u8; 4096];
    let mut closes = 0;
    while closes < expected {
        let bytes = io::Read::read(&mut file, &mut events).expect("read inotify event");
        assert!(bytes > 0, "inotify read must produce a close event");
        let mut offset = 0;
        while offset + 16 <= bytes {
            let mask = u32::from_ne_bytes(
                events[offset + 4..offset + 8]
                    .try_into()
                    .expect("inotify mask bytes"),
            );
            let name_length = u32::from_ne_bytes(
                events[offset + 12..offset + 16]
                    .try_into()
                    .expect("inotify name length bytes"),
            ) as usize;
            offset = offset.saturating_add(16).saturating_add(name_length);
            if mask & IN_CLOSE_NOWRITE != 0 {
                closes += 1;
            }
        }
    }
}

#[cfg(target_os = "linux")]
fn restore_mtime(path: &Path, metadata: &fs::Metadata) {
    let pathname = CString::new(path.to_string_lossy().as_bytes()).expect("valid path");
    let times = [
        Timespec {
            tv_sec: 0,
            tv_nsec: UTIME_OMIT,
        },
        Timespec {
            tv_sec: metadata.mtime(),
            tv_nsec: metadata.mtime_nsec(),
        },
    ];
    let result = unsafe { utimensat(AT_FDCWD, pathname.as_ptr(), times.as_ptr(), 0) };
    assert_eq!(result, 0, "utimensat failed for {}", path.display());
}

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
        self.initialize_with_client_capabilities(
            root,
            initialization_options,
            dynamic_watched_registration,
            false,
        )
    }

    fn initialize_with_action_support(
        &mut self,
        root: &Path,
        initialization_options: Value,
    ) -> Value {
        self.initialize_with_client_capabilities(root, initialization_options, false, true)
    }

    fn initialize_with_resolve_properties(
        &mut self,
        root: &Path,
        initialization_options: Value,
        properties: Value,
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
                        "implementation": {"dynamicRegistration": false},
                        "codeAction": {
                            "dynamicRegistration": false,
                            "dataSupport": true,
                            "disabledSupport": true,
                            "resolveSupport": {"properties": properties}
                        }
                    },
                    "workspace": {
                        "workspaceFolders": true,
                        "workspaceEdit": {"documentChanges": true}
                    }
                }
            }),
        );
        let response = self.response(&id);
        assert!(response.error.is_none(), "initialize failed: {response:?}");
        self.send_notification("initialized", json!({}));
        response.result.expect("initialize result")
    }

    fn initialize_without_document_changes(
        &mut self,
        root: &Path,
        initialization_options: Value,
    ) -> Value {
        self.initialize_with_client_capabilities_and_document_changes(
            root,
            initialization_options,
            false,
            false,
            false,
        )
    }

    fn initialize_with_client_capabilities(
        &mut self,
        root: &Path,
        initialization_options: Value,
        dynamic_watched_registration: bool,
        action_support: bool,
    ) -> Value {
        self.initialize_with_client_capabilities_and_document_changes(
            root,
            initialization_options,
            dynamic_watched_registration,
            action_support,
            true,
        )
    }

    fn initialize_with_client_capabilities_and_document_changes(
        &mut self,
        root: &Path,
        initialization_options: Value,
        dynamic_watched_registration: bool,
        action_support: bool,
        document_changes: bool,
    ) -> Value {
        let root_uri = Url::from_file_path(root).expect("workspace URI");
        let id = RequestId::from("initialize".to_string());
        let code_action_capabilities = if action_support {
            json!({
                "dynamicRegistration": false,
                "dataSupport": true,
                "disabledSupport": true,
                "resolveSupport": {"properties": ["edit"]}
            })
        } else {
            json!({})
        };
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
                        "implementation": {"dynamicRegistration": false},
                        "codeAction": code_action_capabilities
                    },
                    "workspace": {
                        "workspaceFolders": true,
                        "didChangeWatchedFiles": {"dynamicRegistration": dynamic_watched_registration},
                        "workspaceEdit": {"documentChanges": document_changes}
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

fn workspace_edit_uris(edit: &Value) -> HashSet<String> {
    if let Some(changes) = edit["documentChanges"].as_array() {
        return changes
            .iter()
            .filter_map(|change| change["textDocument"]["uri"].as_str().map(str::to_owned))
            .collect();
    }
    edit["changes"]
        .as_object()
        .map(|changes| changes.keys().cloned().collect())
        .unwrap_or_default()
}

fn assert_exact_rename_edits(
    edit: &Value,
    path: &Path,
    source: &str,
    needle: &str,
    occurrences: usize,
    new_name: &str,
) {
    let changes = edit["documentChanges"]
        .as_array()
        .expect("document changes");
    let mut actual = changes
        .iter()
        .flat_map(|change| {
            let uri = change["textDocument"]["uri"]
                .as_str()
                .expect("changed document URI")
                .to_owned();
            change["edits"]
                .as_array()
                .expect("document edits")
                .iter()
                .map(move |edit| {
                    let start = &edit["range"]["start"];
                    let end = &edit["range"]["end"];
                    (
                        uri.clone(),
                        Position::new(
                            start["line"].as_u64().expect("start line") as u32,
                            start["character"].as_u64().expect("start character") as u32,
                        ),
                        Position::new(
                            end["line"].as_u64().expect("end line") as u32,
                            end["character"].as_u64().expect("end character") as u32,
                        ),
                        edit["newText"].as_str().expect("replacement").to_owned(),
                    )
                })
        })
        .collect::<Vec<_>>();
    let mut expected = (0..occurrences)
        .map(|occurrence| {
            let start = position_of(source, needle, occurrence);
            let end = Position::new(
                start.line,
                start.character + needle.encode_utf16().count() as u32,
            );
            (uri(path).to_string(), start, end, new_name.to_owned())
        })
        .collect::<Vec<_>>();
    let sort_edits = |edits: &mut Vec<(String, Position, Position, String)>| {
        edits.sort_by(|left, right| {
            (
                &left.0,
                left.1.line,
                left.1.character,
                left.2.line,
                left.2.character,
                &left.3,
            )
                .cmp(&(
                    &right.0,
                    right.1.line,
                    right.1.character,
                    right.2.line,
                    right.2.character,
                    &right.3,
                ))
        });
    };
    sort_edits(&mut actual);
    sort_edits(&mut expected);
    assert_eq!(actual, expected);
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
fn opening_a_buffer_is_not_rejected_by_unrelated_initial_disk_entries() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path().join("workspace");
    let unrelated = root.join("A_Unrelated.pas");
    let main = root.join("Z_Main.pas");
    write_file(
        &unrelated,
        "unit Unrelated;\ninterface\nimplementation\nend.\n",
    );
    let main_source = "unit Main;\ninterface\nimplementation\nend.\n";

    let mut server = TestServer::launch();
    server.initialize(&root, json!({"maxFiles": 1}));
    let sync_id = RequestId::from("after-initialize".to_string());
    server.send_request(sync_id.clone(), "pascal/unknown", json!({}));
    assert!(server.response(&sync_id).error.is_some());
    write_file(&main, main_source);
    server.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri(&main),
                "languageId": "pascal",
                "version": 1,
                "text": main_source,
            }
        }),
    );
    let diagnostics = server.notification("textDocument/publishDiagnostics");
    assert!(
        diagnostics["diagnostics"]
            .as_array()
            .expect("diagnostics array")
            .iter()
            .all(|diagnostic| !diagnostic["message"]
                .as_str()
                .unwrap_or_default()
                .contains("file limit")),
        "unrelated disk entries must not reject the opened buffer: {diagnostics}"
    );
    server.shutdown();
}

#[test]
fn project_context_binds_same_named_units_to_the_nearest_project() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path().join("workspace");
    let a_main = root.join("A/Main.pas");
    let a_unit = root.join("A/Shared.pas");
    let b_main = root.join("B/Main.pas");
    let b_unit = root.join("B/Shared.pas");
    let main_source = "unit Main;\ninterface\nuses Shared;\nimplementation\nprocedure Run;\nbegin\n  Routine;\nend;\nend.\n";
    let unit_source = "unit Shared;\ninterface\nprocedure Routine;\nimplementation\nprocedure Routine; begin end;\nend.\n";
    write_file(&a_main, main_source);
    write_file(&a_unit, unit_source);
    write_file(&b_main, main_source);
    write_file(&b_unit, unit_source);
    write_file(
        &root.join("A/App.dproj"),
        "<Project><PropertyGroup><MainSource>Main.pas</MainSource></PropertyGroup></Project>",
    );
    write_file(
        &root.join("B/App.dproj"),
        "<Project><PropertyGroup><MainSource>Main.pas</MainSource></PropertyGroup></Project>",
    );

    let mut server = TestServer::launch();
    server.initialize(&root, Value::Null);
    for (id, main, unit) in [
        ("project-a", &a_main, &a_unit),
        ("project-b", &b_main, &b_unit),
    ] {
        let request_id = RequestId::from(id.to_string());
        server.send_request(
            request_id.clone(),
            "textDocument/declaration",
            navigation_params(main, main_source, "Routine", 0),
        );
        let locations = result_locations(server.response(&request_id));
        assert_eq!(locations.len(), 1, "{id} should have one bound result");
        assert_eq!(locations[0]["uri"], uri(unit).to_string());
    }
    server.shutdown();
}

#[test]
fn open_dependency_keeps_its_selected_project_context_across_requests() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path().join("workspace");
    let shared = root.join("A/src/Shared.pas");
    let a_config = root.join("A/lib/Config.pas");
    let b_config = root.join("B/lib/Config.pas");
    let b_main = root.join("B/Main.pas");
    let shared_source = "unit Shared;\ninterface\nuses Config;\nprocedure Run;\nimplementation\nprocedure Run;\nbegin\n  Routine;\nend;\nend.\n";
    let b_main_source = "unit Main;\ninterface\nuses Shared;\nimplementation\nprocedure Run;\nbegin\n  Routine;\nend;\nend.\n";
    let config_source = "unit Config;\ninterface\nprocedure Routine;\nimplementation\nprocedure Routine; begin end;\nend.\n";
    write_file(&shared, shared_source);
    write_file(&a_config, config_source);
    write_file(&b_config, config_source);
    write_file(&b_main, b_main_source);
    write_file(
        &root.join("A/App.dproj"),
        "<Project><PropertyGroup><MainSource>App.dpr</MainSource></PropertyGroup></Project>",
    );
    write_file(
        &root.join("A/App.dpr"),
        "program App; uses Config in 'lib/Config.pas'; begin end.\n",
    );
    write_file(
        &root.join("B/App.dproj"),
        "<Project><PropertyGroup><MainSource>App.dpr</MainSource></PropertyGroup></Project>",
    );
    write_file(
        &root.join("B/App.dpr"),
        "program App; uses Shared in '../A/src/Shared.pas', Config in 'lib/Config.pas'; begin end.\n",
    );

    let mut server = TestServer::launch();
    server.initialize(&root, Value::Null);
    server.send_notification(
        "textDocument/didOpen",
        json!({"textDocument": {"uri": uri(&shared), "languageId": "pascal", "version": 1, "text": shared_source}}),
    );
    let _ = server.notification("textDocument/publishDiagnostics");

    let b_request_id = RequestId::from("b-context".to_string());
    server.send_request(
        b_request_id.clone(),
        "textDocument/declaration",
        navigation_params(&b_main, b_main_source, "Shared", 0),
    );
    let b_locations = result_locations(server.response(&b_request_id));
    assert_eq!(b_locations.len(), 1);
    assert_eq!(b_locations[0]["uri"], uri(&shared).to_string());

    let a_request_id = RequestId::from("a-context".to_string());
    server.send_request(
        a_request_id.clone(),
        "textDocument/declaration",
        navigation_params(&shared, shared_source, "Routine", 0),
    );
    let a_locations = result_locations(server.response(&a_request_id));
    assert_eq!(a_locations.len(), 1);
    assert_eq!(a_locations[0]["uri"], uri(&a_config).to_string());
    server.shutdown();
}

#[test]
fn projectless_context_watches_each_file_ancestors_for_new_nearer_projects() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path().join("workspace");
    let a_main = root.join("A/Main.pas");
    let b_main = root.join("B/Main.pas");
    let a_config = root.join("A/Config.pas");
    let b_config = root.join("B/Config.pas");
    let selected_config = root.join("A/lib/Config.pas");
    let main_source = "unit Main;\ninterface\nuses Config;\nimplementation\nprocedure Run;\nbegin\n  Routine;\nend;\nend.\n";
    let config_source = "unit Config;\ninterface\nprocedure Routine;\nimplementation\nprocedure Routine; begin end;\nend.\n";
    write_file(&a_main, main_source);
    write_file(&b_main, main_source);
    write_file(&a_config, config_source);
    write_file(&b_config, config_source);
    write_file(&selected_config, config_source);

    let mut server = TestServer::launch();
    server.initialize(&root, Value::Null);
    for (id, main, expected) in [
        ("projectless-a", &a_main, &a_config),
        ("projectless-b", &b_main, &b_config),
    ] {
        let request_id = RequestId::from(id.to_string());
        server.send_request(
            request_id.clone(),
            "textDocument/declaration",
            navigation_params(main, main_source, "Routine", 0),
        );
        let locations = result_locations(server.response(&request_id));
        assert_eq!(locations.len(), 1);
        assert_eq!(locations[0]["uri"], uri(expected).to_string());
    }

    write_file(
        &root.join("A/App.dproj"),
        "<Project><PropertyGroup><MainSource>App.dpr</MainSource></PropertyGroup></Project>",
    );
    write_file(
        &root.join("A/App.dpr"),
        "program App; uses Config in 'lib/Config.pas'; begin end.\n",
    );
    let request_id = RequestId::from("nearer-project".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/declaration",
        navigation_params(&a_main, main_source, "Routine", 0),
    );
    let locations = result_locations(server.response(&request_id));
    assert_eq!(locations.len(), 1);
    assert_eq!(locations[0]["uri"], uri(&selected_config).to_string());
    server.shutdown();
}

#[test]
fn navigation_loads_transitive_typed_dependencies_and_bounds_circular_uses() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path().join("workspace");
    let main = root.join("Main.pas");
    let direct = root.join("Direct.pas");
    let types = root.join("Types.pas");
    let cycle_a = root.join("CycleA.pas");
    let cycle_b = root.join("CycleB.pas");
    let main_source = "unit Main;\ninterface\nuses Direct;\nimplementation\nprocedure Run;\nbegin\n  Item.Field;\nend;\nend.\n";
    let direct_source = "unit Direct;\ninterface\nuses Types, CycleA;\nvar\n  Item: Types.TThing;\nimplementation\nuses Main;\nend.\n";
    let types_source = "unit Types;\ninterface\ntype\n  TThing = class\n    Field: Integer;\n  end;\nimplementation\nend.\n";
    write_file(&main, main_source);
    write_file(&direct, direct_source);
    write_file(&types, types_source);
    write_file(
        &cycle_a,
        "unit CycleA;\ninterface\nuses CycleB;\nprocedure A;\nimplementation\nprocedure A; begin end;\nend.\n",
    );
    write_file(
        &cycle_b,
        "unit CycleB;\ninterface\nuses CycleA;\nprocedure B;\nimplementation\nprocedure B; begin end;\nend.\n",
    );

    let mut server = TestServer::launch();
    server.initialize(&root, Value::Null);
    let request_id = RequestId::from("transitive".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/declaration",
        navigation_params(&main, main_source, "Field", 0),
    );
    let locations = result_locations(server.response(&request_id));
    assert_eq!(locations.len(), 1);
    assert_eq!(locations[0]["uri"], uri(&types).to_string());
    server.shutdown();
}

#[test]
fn project_namespace_and_unit_aliases_bind_qualified_imports() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path().join("workspace");
    let main = root.join("Main.pas");
    let provider = root.join("Vendor.Core.pas");
    let main_source = "unit Main;\ninterface\nuses Legacy;\nimplementation\nprocedure Run;\nbegin\n  LegacyRoutine;\nend;\nend.\n";
    let provider_source = "unit Vendor.Core;\ninterface\nprocedure LegacyRoutine;\nimplementation\nprocedure LegacyRoutine; begin end;\nend.\n";
    write_file(&main, main_source);
    write_file(&provider, provider_source);
    write_file(
        &root.join("App.dproj"),
        "<Project><PropertyGroup><MainSource>Main.pas</MainSource><DCC_Namespace>Vendor</DCC_Namespace><DCC_UnitAlias>Legacy=Vendor.Core</DCC_UnitAlias></PropertyGroup></Project>",
    );

    let mut server = TestServer::launch();
    server.initialize(&root, Value::Null);
    let request_id = RequestId::from("namespace-alias".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/declaration",
        navigation_params(&main, main_source, "LegacyRoutine", 0),
    );
    let locations = result_locations(server.response(&request_id));
    assert_eq!(locations.len(), 1);
    assert_eq!(locations[0]["uri"], uri(&provider).to_string());
    server.shutdown();
}

#[test]
fn project_namespace_resolves_unqualified_unit_to_a_qualified_filename() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path().join("workspace");
    let main = root.join("Main.pas");
    let provider = root.join("Vendor.Core.pas");
    let main_source = "unit Main;\ninterface\nuses Core;\nimplementation\nprocedure Run;\nbegin\n  CoreRoutine;\nend;\nend.\n";
    let provider_source = "unit Vendor.Core;\ninterface\nprocedure CoreRoutine;\nimplementation\nprocedure CoreRoutine; begin end;\nend.\n";
    write_file(&main, main_source);
    write_file(&provider, provider_source);
    write_file(
        &root.join("App.dproj"),
        "<Project><PropertyGroup><MainSource>Main.pas</MainSource><DCC_Namespace>Vendor</DCC_Namespace></PropertyGroup></Project>",
    );

    let mut server = TestServer::launch();
    server.initialize(&root, Value::Null);
    let request_id = RequestId::from("namespace-qualified".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/declaration",
        navigation_params(&main, main_source, "CoreRoutine", 0),
    );
    let locations = result_locations(server.response(&request_id));
    assert_eq!(locations.len(), 1);
    assert_eq!(locations[0]["uri"], uri(&provider).to_string());
    server.shutdown();
}

#[test]
fn open_document_context_survives_metadata_invalidation_before_dependency_load() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path().join("workspace");
    let shared = root.join("A/src/Shared.pas");
    let a_config = root.join("A/lib/Config.pas");
    let b_config = root.join("B/lib/Config.pas");
    let b_main = root.join("B/Main.pas");
    let a_project = root.join("A/App.dproj");
    let shared_source = "unit Shared;\ninterface\nuses Config;\nprocedure Run;\nimplementation\nprocedure Run;\nbegin\n  Routine;\nend;\nend.\n";
    let b_main_source = "unit Main;\ninterface\nuses Shared;\nimplementation\nprocedure Run;\nbegin\n  Routine;\nend;\nend.\n";
    let config_source = "unit Config;\ninterface\nprocedure Routine;\nimplementation\nprocedure Routine; begin end;\nend.\n";
    write_file(&shared, shared_source);
    write_file(&a_config, config_source);
    write_file(&b_config, config_source);
    write_file(&b_main, b_main_source);
    write_file(
        &a_project,
        "<Project><PropertyGroup><MainSource>App.dpr</MainSource></PropertyGroup></Project>",
    );
    write_file(
        &root.join("A/App.dpr"),
        "program App; uses Config in 'lib/Config.pas'; begin end.\n",
    );
    write_file(
        &root.join("B/App.dproj"),
        "<Project><PropertyGroup><MainSource>App.dpr</MainSource></PropertyGroup></Project>",
    );
    write_file(
        &root.join("B/App.dpr"),
        "program App; uses Shared in '../A/src/Shared.pas', Config in 'lib/Config.pas'; begin end.\n",
    );

    let mut server = TestServer::launch();
    server.initialize(&root, Value::Null);
    server.send_notification(
        "textDocument/didOpen",
        json!({"textDocument": {"uri": uri(&shared), "languageId": "pascal", "version": 1, "text": shared_source}}),
    );
    let _ = server.notification("textDocument/publishDiagnostics");

    let before_id = RequestId::from("open-context-before-invalidation".to_string());
    server.send_request(
        before_id.clone(),
        "textDocument/declaration",
        navigation_params(&shared, shared_source, "Routine", 0),
    );
    let before_locations = result_locations(server.response(&before_id));
    assert_eq!(before_locations.len(), 1);
    assert_eq!(before_locations[0]["uri"], uri(&a_config).to_string());

    write_file(
        &a_project,
        "<Project><PropertyGroup><MainSource>App.dpr</MainSource><DCC_Define>AFTER_EDIT</DCC_Define></PropertyGroup></Project>",
    );
    server.send_notification(
        "workspace/didChangeWatchedFiles",
        json!({"changes": [{"uri": uri(&a_project), "type": 2}]}),
    );

    let dependency_id = RequestId::from("dependency-context".to_string());
    server.send_request(
        dependency_id.clone(),
        "textDocument/declaration",
        navigation_params(&b_main, b_main_source, "Shared", 0),
    );
    let dependency_locations = result_locations(server.response(&dependency_id));
    assert_eq!(dependency_locations.len(), 1);
    assert_eq!(dependency_locations[0]["uri"], uri(&shared).to_string());

    let after_id = RequestId::from("open-context-after-invalidation".to_string());
    server.send_request(
        after_id.clone(),
        "textDocument/declaration",
        navigation_params(&shared, shared_source, "Routine", 0),
    );
    let after_locations = result_locations(server.response(&after_id));
    assert_eq!(after_locations.len(), 1);
    assert_eq!(after_locations[0]["uri"], uri(&a_config).to_string());
    server.shutdown();
}

#[test]
fn project_namespace_prefers_exact_filename_over_qualified_candidates() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path().join("workspace");
    let main = root.join("Main.pas");
    let exact = root.join("Core.pas");
    let namespaced = root.join("Vendor.Core.pas");
    let main_source = "unit Main;\ninterface\nuses Core;\nimplementation\nprocedure Run;\nbegin\n  Routine;\nend;\nend.\n";
    let exact_source = "unit Core;\ninterface\nprocedure Routine;\nimplementation\nprocedure Routine; begin end;\nend.\n";
    let namespaced_source = "unit Vendor.Core;\ninterface\nprocedure Routine;\nimplementation\nprocedure Routine; begin end;\nend.\n";
    write_file(&main, main_source);
    write_file(&exact, exact_source);
    write_file(&namespaced, namespaced_source);
    write_file(
        &root.join("App.dproj"),
        "<Project><PropertyGroup><MainSource>Main.pas</MainSource><DCC_Namespace>Vendor;Other</DCC_Namespace></PropertyGroup></Project>",
    );

    let mut server = TestServer::launch();
    server.initialize(&root, Value::Null);
    let request_id = RequestId::from("namespace-exact".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/declaration",
        navigation_params(&main, main_source, "Routine", 0),
    );
    let locations = result_locations(server.response(&request_id));
    assert_eq!(locations.len(), 1);
    assert_eq!(locations[0]["uri"], uri(&exact).to_string());
    server.shutdown();
}

#[test]
fn project_namespace_order_prefers_the_first_qualified_candidate() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path().join("workspace");
    let main = root.join("Main.pas");
    let vendor = root.join("Vendor.Core.pas");
    let other = root.join("Other.Core.pas");
    let main_source = "unit Main;\ninterface\nuses Core;\nimplementation\nprocedure Run;\nbegin\n  Routine;\nend;\nend.\n";
    let vendor_source = "unit Vendor.Core;\ninterface\nprocedure Routine;\nimplementation\nprocedure Routine; begin end;\nend.\n";
    let other_source = "unit Other.Core;\ninterface\nprocedure Routine;\nimplementation\nprocedure Routine; begin end;\nend.\n";
    write_file(&main, main_source);
    write_file(&vendor, vendor_source);
    write_file(&other, other_source);
    write_file(
        &root.join("App.dproj"),
        "<Project><PropertyGroup><MainSource>Main.pas</MainSource><DCC_Namespace>Vendor;Other</DCC_Namespace></PropertyGroup></Project>",
    );

    let mut server = TestServer::launch();
    server.initialize(&root, Value::Null);
    let request_id = RequestId::from("namespace-order".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/declaration",
        navigation_params(&main, main_source, "Routine", 0),
    );
    let locations = result_locations(server.response(&request_id));
    assert_eq!(locations.len(), 1);
    assert_eq!(locations[0]["uri"], uri(&vendor).to_string());
    server.shutdown();
}

#[test]
fn accepted_open_dependency_is_resolved_without_a_disk_entry() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path().join("workspace");
    let main = root.join("Main.pas");
    let provider = root.join("Provider.pas");
    let main_source = "unit Main;\ninterface\nuses Provider;\nimplementation\nprocedure Run;\nbegin\n  ProviderRoutine;\nend;\nend.\n";
    let provider_source = "unit Provider;\ninterface\nprocedure ProviderRoutine;\nimplementation\nprocedure ProviderRoutine; begin end;\nend.\n";
    write_file(&main, main_source);

    let mut server = TestServer::launch();
    server.initialize(&root, Value::Null);
    server.send_notification(
        "textDocument/didOpen",
        json!({"textDocument": {"uri": uri(&provider), "languageId": "pascal", "version": 1, "text": provider_source}}),
    );
    let _ = server.notification("textDocument/publishDiagnostics");
    let request_id = RequestId::from("open-provider".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/declaration",
        navigation_params(&main, main_source, "ProviderRoutine", 0),
    );
    let locations = result_locations(server.response(&request_id));
    assert_eq!(locations.len(), 1);
    assert_eq!(locations[0]["uri"], uri(&provider).to_string());
    server.shutdown();
}

#[test]
fn rejected_open_dependency_without_a_disk_entry_is_not_resolved() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path().join("workspace");
    let main = root.join("Main.pas");
    let provider = root.join("Provider.pas");
    let main_source = "unit Main;\ninterface\nuses Provider;\nimplementation\nprocedure Run;\nbegin\n  ProviderRoutine;\nend;\nend.\n";
    let provider_source = "unit Provider;\ninterface\nprocedure ProviderRoutine;\nimplementation\nprocedure ProviderRoutine; begin end;\nend.\n";
    write_file(&main, main_source);

    let mut server = TestServer::launch();
    server.initialize(&root, json!({"maxFileBytes": 64}));
    server.send_notification(
        "textDocument/didOpen",
        json!({"textDocument": {"uri": uri(&provider), "languageId": "pascal", "version": 1, "text": provider_source}}),
    );
    let _ = server.notification("textDocument/publishDiagnostics");
    let request_id = RequestId::from("rejected-open-provider".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/declaration",
        navigation_params(&main, main_source, "ProviderRoutine", 0),
    );
    assert!(result_locations(server.response(&request_id)).is_empty());
    server.shutdown();
}

#[test]
fn project_explicit_dpr_unit_paths_can_load_sources_outside_workspace_root() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let project_root = temp.path().join("project");
    let main = project_root.join("App.dpr");
    let external = temp.path().join("external/ExternalUnit.pas");
    let main_source = "program App;\nuses ExternalUnit in '..\\external\\ExternalUnit.pas';\nbegin\n  ExternalRoutine;\nend.\n";
    let external_source = "unit ExternalUnit;\ninterface\nprocedure ExternalRoutine;\nimplementation\nprocedure ExternalRoutine; begin end;\nend.\n";
    write_file(&main, main_source);
    write_file(&external, external_source);
    write_file(
        &project_root.join("App.dproj"),
        "<Project><PropertyGroup><MainSource>App.dpr</MainSource></PropertyGroup></Project>",
    );

    let mut server = TestServer::launch();
    server.initialize(&project_root, Value::Null);
    let request_id = RequestId::from("external-dpr".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/declaration",
        navigation_params(&main, main_source, "ExternalRoutine", 0),
    );
    let locations = result_locations(server.response(&request_id));
    assert_eq!(locations.len(), 1);
    assert_eq!(locations[0]["uri"], uri(&external).to_string());
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
fn projectless_filename_catalogue_revalidates_new_nested_sources_without_watchers() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path().join("workspace");
    let nested = root.join("library");
    let main = root.join("Main.pas");
    let provider = nested.join("Nested.pas");
    let main_source = "unit Main;\ninterface\nuses Nested;\nimplementation\nprocedure Run;\nbegin\n  NestedRoutine;\nend;\nend.\n";
    write_file(&main, main_source);
    fs::create_dir_all(&nested).expect("create nested source directory");

    let mut server = TestServer::launch();
    server.initialize(&root, Value::Null);
    let request_id = RequestId::from("before-new-file".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/declaration",
        navigation_params(&main, main_source, "NestedRoutine", 0),
    );
    assert!(result_locations(server.response(&request_id)).is_empty());

    write_file(
        &provider,
        "unit Nested;\ninterface\nprocedure NestedRoutine;\nimplementation\nprocedure NestedRoutine; begin end;\nend.\n",
    );
    let request_id = RequestId::from("after-new-file".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/declaration",
        navigation_params(&main, main_source, "NestedRoutine", 0),
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

#[test]
fn project_package_contains_resolves_case_insensitive_types_without_parsing_unrelated_sources() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path().join("workspace");
    let main = root.join("DatabaseManager.pas");
    let package = root.join("multidev/MultidevD10.dpk");
    let provider = root.join("multidev/src/MDIBDatabase.pas");
    let unrelated = root.join("multidev/src/Unrelated.pas");
    let main_source = "unit DatabaseManager;\ninterface\nuses mdibdatabase;\nimplementation\nprocedure Run;\nvar\n  Database: TMDIBDatabase;\nbegin\n  Database := TMDIBDatabase.Create;\nend;\nend.\n";
    let provider_source = "unit MDIBDatabase;\ninterface\ntype\n  TMDIBDatabase = class\n  end;\nimplementation\nend.\n";
    write_file(&main, main_source);
    write_file(&provider, provider_source);
    write_file(
        &unrelated,
        "unit Unrelated;\ninterface\nprocedure NeverUsed;\nimplementation\nprocedure NeverUsed; begin end;\nend.\n",
    );
    write_file(
        &package,
        "package LegacyHeaderName;\ncontains\n  MDIBDatabase in 'src\\MDIBDatabase.pas';\nend.\n",
    );
    write_file(
        &root.join("multidev/MultidevD10.dproj"),
        "this project metadata is intentionally ignored when the same-stem DPK exists",
    );
    write_file(
        &root.join("WebQuery.dproj"),
        "<Project><PropertyGroup><MainSource>DatabaseManager.pas</MainSource><DCC_UsePackage>multidevd10</DCC_UsePackage></PropertyGroup></Project>",
    );

    let main_uri = uri(&main);
    let mut workspace = Workspace::new(vec![root], WorkspaceOptions::default());
    let locations = workspace.navigate(
        &main_uri,
        position_of(main_source, "TMDIBDatabase", 0),
        NavigationTarget::Definition,
    );

    assert_eq!(locations.len(), 1);
    assert_eq!(locations[0].uri, uri(&provider));
    assert_eq!(workspace.parsed_document_count(), 2);
}

#[test]
fn direct_source_paths_override_package_contains_mappings() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path().join("workspace");
    let direct = root.join("direct/Override.pas");
    let package = root.join("packages/OverridePackage.dpk");
    let packaged = root.join("packages/Override.pas");
    let main = root.join("Main.pas");
    let main_source = "unit Main;\ninterface\nuses Override;\nimplementation\nprocedure Run;\nbegin\n  DirectRoutine;\nend;\nend.\n";
    write_file(&main, main_source);
    write_file(
        &direct,
        "unit Override;\ninterface\nprocedure DirectRoutine;\nimplementation\nprocedure DirectRoutine; begin end;\nend.\n",
    );
    write_file(
        &packaged,
        "unit Override;\ninterface\nprocedure PackagedRoutine;\nimplementation\nprocedure PackagedRoutine; begin end;\nend.\n",
    );
    write_file(
        &package,
        "package OverridePackage;\ncontains\n  Override in 'Override.pas';\nend.\n",
    );
    write_file(
        &root.join("App.dproj"),
        "<Project><PropertyGroup><MainSource>Main.pas</MainSource><DCC_UsePackage>OverridePackage</DCC_UsePackage></PropertyGroup></Project>",
    );

    let mut workspace = Workspace::new(
        vec![root.clone()],
        WorkspaceOptions {
            source_paths: vec!["direct".to_string()],
            ..WorkspaceOptions::default()
        },
    );
    let locations = workspace.navigate(
        &uri(&main),
        position_of(main_source, "DirectRoutine", 0),
        NavigationTarget::Declaration,
    );

    assert_eq!(locations.len(), 1);
    assert_eq!(locations[0].uri, uri(&direct));
    assert_eq!(workspace.parsed_document_count(), 2);
}

#[test]
fn missing_compiled_only_package_is_reported_only_when_an_import_cannot_resolve() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path().join("workspace");
    let main = root.join("Main.pas");
    let main_source = "unit Main;\ninterface\nuses MissingUnit;\nimplementation\nprocedure Run;\nbegin\n  MissingRoutine;\nend;\nend.\n";
    write_file(&main, main_source);
    write_file(
        &root.join("App.dproj"),
        "<Project><PropertyGroup><MainSource>Main.pas</MainSource><DCC_UsePackage>MissingCompiledPackage</DCC_UsePackage></PropertyGroup></Project>",
    );

    let context = ProjectContext::discover(
        &main,
        std::slice::from_ref(&root),
        &pascal_lsp::ProjectOptions::default(),
    )
    .expect("discover project context");
    assert!(context.warnings.is_empty());

    let mut workspace = Workspace::new(vec![root], WorkspaceOptions::default());
    assert!(
        workspace
            .navigate(
                &uri(&main),
                position_of(main_source, "MissingRoutine", 0),
                NavigationTarget::Declaration,
            )
            .is_empty()
    );
    assert!(
        workspace
            .warnings()
            .iter()
            .any(|warning| warning.contains("missingcompiledpackage"))
    );
}

#[test]
fn package_name_in_an_unrelated_dpk_is_not_used_without_an_exact_filename_match() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path().join("workspace");
    let main = root.join("Main.pas");
    let provider = root.join("unrelated/HiddenUnit.pas");
    let descriptor = root.join("unrelated/InstalledName.dpk");
    let main_source = "unit Main;\ninterface\nuses HiddenUnit;\nimplementation\nprocedure Run;\nbegin\n  HiddenRoutine;\nend;\nend.\n";
    write_file(&main, main_source);
    write_file(
        &provider,
        "unit HiddenUnit;\ninterface\nprocedure HiddenRoutine;\nimplementation\nprocedure HiddenRoutine; begin end;\nend.\n",
    );
    write_file(
        &descriptor,
        "package RequestedPackage;\ncontains\n  HiddenUnit in 'HiddenUnit.pas';\nend.\n",
    );
    write_file(
        &root.join("App.dproj"),
        "<Project><PropertyGroup><MainSource>Main.pas</MainSource><DCC_UsePackage>RequestedPackage</DCC_UsePackage></PropertyGroup></Project>",
    );

    let mut workspace = Workspace::new(vec![root], WorkspaceOptions::default());
    assert!(
        workspace
            .navigate(
                &uri(&main),
                position_of(main_source, "HiddenRoutine", 0),
                NavigationTarget::Declaration,
            )
            .is_empty()
    );
    assert!(workspace.warnings().iter().any(|warning| {
        warning.contains("source for package requestedpackage was not found under")
    }));
}

#[test]
fn package_contains_changes_are_seen_without_file_watcher_notifications() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path().join("workspace");
    let main = root.join("Main.pas");
    let package = root.join("Package.dpk");
    let old_unit = root.join("packages/OldUnit.pas");
    let new_unit = root.join("packages/NewUnit.pas");
    let main_source = "unit Main;\ninterface\nuses NewUnit;\nimplementation\nprocedure Run;\nbegin\n  NewRoutine;\nend;\nend.\n";
    write_file(&main, main_source);
    write_file(
        &old_unit,
        "unit OldUnit;\ninterface\nprocedure OldRoutine;\nimplementation\nprocedure OldRoutine; begin end;\nend.\n",
    );
    write_file(
        &new_unit,
        "unit NewUnit;\ninterface\nprocedure NewRoutine;\nimplementation\nprocedure NewRoutine; begin end;\nend.\n",
    );
    write_file(
        &package,
        "package Package;\ncontains\n  OldUnit in 'packages/OldUnit.pas';\nend.\n",
    );
    write_file(
        &root.join("App.dproj"),
        "<Project><PropertyGroup><MainSource>Main.pas</MainSource><DCC_UsePackage>Package</DCC_UsePackage></PropertyGroup></Project>",
    );

    let mut workspace = Workspace::new(vec![root], WorkspaceOptions::default());
    assert!(
        workspace
            .navigate(
                &uri(&main),
                position_of(main_source, "NewRoutine", 0),
                NavigationTarget::Declaration,
            )
            .is_empty()
    );

    write_file(
        &package,
        "package Package;\ncontains\n  NewUnit in 'packages/NewUnit.pas';\nend;\n",
    );
    let locations = workspace.navigate(
        &uri(&main),
        position_of(main_source, "NewRoutine", 0),
        NavigationTarget::Declaration,
    );

    assert_eq!(locations.len(), 1);
    assert_eq!(locations[0].uri, uri(&new_unit));
}

#[test]
fn package_dproj_import_metadata_changes_are_seen_with_and_without_file_events() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path().join("workspace");
    let main = root.join("Main.pas");
    let package_project = root.join("packages/Package.dproj");
    let package_source = root.join("packages/PackageMain.dpk");
    let mappings = root.join("packages/Mappings.optset");
    let version_one = root.join("packages/v1/MappedUnit.pas");
    let version_two = root.join("packages/version-two/MappedUnit.pas");
    let version_three = root.join("packages/version-three/MappedUnit.pas");
    let main_source = "unit Main;\ninterface\nuses MappedUnit;\nimplementation\nprocedure Run;\nbegin\n  MappedRoutine;\nend;\nend.\n";
    let provider_source = "unit MappedUnit;\ninterface\nprocedure MappedRoutine;\nimplementation\nprocedure MappedRoutine; begin end;\nend.\n";
    write_file(&main, main_source);
    write_file(&version_one, provider_source);
    write_file(&version_two, provider_source);
    write_file(&version_three, provider_source);
    write_file(&package_source, "package PackageMain;\ncontains\nend.\n");
    write_file(
        &mappings,
        "<Project><ItemGroup><DCCReference Include=\"v1/MappedUnit.pas\" /></ItemGroup></Project>",
    );
    write_file(
        &package_project,
        "<Project><PropertyGroup><MainSource>PackageMain.dpk</MainSource></PropertyGroup><Import Project=\"Mappings.optset\" /></Project>",
    );
    write_file(
        &root.join("App.dproj"),
        "<Project><PropertyGroup><MainSource>Main.pas</MainSource><DCC_UsePackage>Package</DCC_UsePackage></PropertyGroup></Project>",
    );

    let main_uri = uri(&main);
    let mappings_uri = uri(&mappings);
    let mut workspace = Workspace::new(vec![root], WorkspaceOptions::default());
    let first = workspace.navigate(
        &main_uri,
        position_of(main_source, "MappedRoutine", 0),
        NavigationTarget::Declaration,
    );
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].uri, uri(&version_one));

    write_file(
        &mappings,
        "<Project><ItemGroup><DCCReference Include=\"version-two/MappedUnit.pas\" /></ItemGroup></Project>",
    );
    let without_event = workspace.navigate(
        &main_uri,
        position_of(main_source, "MappedRoutine", 0),
        NavigationTarget::Declaration,
    );
    assert_eq!(without_event.len(), 1);
    assert_eq!(without_event[0].uri, uri(&version_two));

    write_file(
        &mappings,
        "<Project><ItemGroup><DCCReference Include=\"version-three/MappedUnit.pas\" /></ItemGroup></Project>",
    );
    workspace.file_event(&mappings_uri, FileChange::Changed);
    let with_event = workspace.navigate(
        &main_uri,
        position_of(main_source, "MappedRoutine", 0),
        NavigationTarget::Declaration,
    );
    assert_eq!(with_event.len(), 1);
    assert_eq!(with_event[0].uri, uri(&version_three));
}

fn exercise_missing_package_import_lifecycle(with_file_events: bool, exists_guard: bool) {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path().join("workspace");
    let main = root.join("Main.pas");
    let package_project = root.join("packages/Package.dproj");
    let package_source = root.join("packages/PackageMain.dpk");
    let mappings = root.join("packages/Mappings.optset");
    let version_one = root.join("packages/v1/MappedUnit.pas");
    let version_two = root.join("packages/v2/MappedUnit.pas");
    let main_source = "unit Main;\ninterface\nuses MappedUnit;\nimplementation\nprocedure Run;\nbegin\n  MappedRoutine;\nend;\nend.\n";
    let provider_source = "unit MappedUnit;\ninterface\nprocedure MappedRoutine;\nimplementation\nprocedure MappedRoutine; begin end;\nend.\n";
    let import = if exists_guard {
        "<Import Project=\"Mappings.optset\" Condition=\"Exists('Mappings.optset')\" />"
    } else {
        "<Import Project=\"Mappings.optset\" />"
    };
    write_file(&main, main_source);
    write_file(&version_one, provider_source);
    write_file(&version_two, provider_source);
    write_file(&package_source, "package PackageMain;\ncontains\nend.\n");
    write_file(
        &package_project,
        &format!(
            "<Project><PropertyGroup><MainSource>PackageMain.dpk</MainSource></PropertyGroup>{import}</Project>"
        ),
    );
    write_file(
        &root.join("App.dproj"),
        "<Project><PropertyGroup><MainSource>Main.pas</MainSource><DCC_UsePackage>Package</DCC_UsePackage></PropertyGroup></Project>",
    );

    let main_uri = uri(&main);
    let mappings_uri = uri(&mappings);
    let mut workspace = Workspace::new(vec![root], WorkspaceOptions::default());
    assert!(
        workspace
            .navigate(
                &main_uri,
                position_of(main_source, "MappedRoutine", 0),
                NavigationTarget::Declaration,
            )
            .is_empty()
    );

    write_file(
        &mappings,
        "<Project><ItemGroup><DCCReference Include=\"v1/MappedUnit.pas\" /></ItemGroup></Project>",
    );
    if with_file_events {
        workspace.file_event(&mappings_uri, FileChange::Created);
    }
    let created = workspace.navigate(
        &main_uri,
        position_of(main_source, "MappedRoutine", 0),
        NavigationTarget::Declaration,
    );
    assert_eq!(created.len(), 1);
    assert_eq!(created[0].uri, uri(&version_one));

    fs::remove_file(&mappings).expect("delete imported optset");
    if with_file_events {
        workspace.file_event(&mappings_uri, FileChange::Deleted);
    }
    assert!(
        workspace
            .navigate(
                &main_uri,
                position_of(main_source, "MappedRoutine", 0),
                NavigationTarget::Declaration,
            )
            .is_empty()
    );

    write_file(
        &mappings,
        "<Project><ItemGroup><DCCReference Include=\"v2/MappedUnit.pas\" /></ItemGroup></Project>",
    );
    if with_file_events {
        workspace.file_event(&mappings_uri, FileChange::Created);
    }
    let restored = workspace.navigate(
        &main_uri,
        position_of(main_source, "MappedRoutine", 0),
        NavigationTarget::Declaration,
    );
    assert_eq!(restored.len(), 1);
    assert_eq!(restored[0].uri, uri(&version_two));
}

#[test]
fn missing_package_imports_revalidate_on_create_delete_restore_with_and_without_events() {
    for with_file_events in [false, true] {
        for exists_guard in [false, true] {
            exercise_missing_package_import_lifecycle(with_file_events, exists_guard);
        }
    }
}

#[test]
fn package_lookup_limit_does_not_return_a_partial_unique_match() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path().join("workspace");
    let main = root.join("Main.pas");
    let first_provider = root.join("packages/Package000/SharedUnit.pas");
    let last_provider = root.join("packages/Package256/SharedUnit.pas");
    let main_source = "unit Main;\ninterface\nuses SharedUnit;\nimplementation\nprocedure Run;\nbegin\n  SharedRoutine;\nend;\nend.\n";
    let provider_source = "unit SharedUnit;\ninterface\nprocedure SharedRoutine;\nimplementation\nprocedure SharedRoutine; begin end;\nend.\n";
    write_file(&main, main_source);
    write_file(&first_provider, provider_source);
    write_file(&last_provider, provider_source);

    let mut package_names = Vec::new();
    for index in 0..=256 {
        let package_name = format!("Package{index:03}");
        package_names.push(package_name.clone());
        let descriptor = root
            .join("packages")
            .join(&package_name)
            .join(format!("{package_name}.dpk"));
        let contents = if index == 0 || index == 256 {
            "package {name};\ncontains\n  SharedUnit in 'SharedUnit.pas';\nend.\n"
                .replace("{name}", &package_name)
        } else {
            format!("package {package_name};\ncontains\nend.\n")
        };
        write_file(&descriptor, &contents);
    }
    write_file(
        &root.join("App.dproj"),
        &format!(
            "<Project><PropertyGroup><MainSource>Main.pas</MainSource><DCC_UsePackage>{}</DCC_UsePackage></PropertyGroup></Project>",
            package_names.join(";"),
        ),
    );

    let mut workspace = Workspace::new(vec![root], WorkspaceOptions::default());
    assert!(
        workspace
            .navigate(
                &uri(&main),
                position_of(main_source, "SharedRoutine", 0),
                NavigationTarget::Declaration,
            )
            .is_empty()
    );
    assert!(
        workspace
            .warnings()
            .iter()
            .any(|warning| warning.contains("named package lookup limit (256)"))
    );
}

#[test]
fn package_unit_candidate_limit_does_not_return_a_partial_unique_match() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path().join("workspace");
    let main = root.join("Main.pas");
    let provider = root.join("valid/RequestedUnit.pas");
    let package = root.join("RequestedPackage.dpk");
    let main_source = "unit Main;\ninterface\nuses RequestedUnit;\nimplementation\nprocedure Run;\nbegin\n  RequestedRoutine;\nend;\nend.\n";
    write_file(&main, main_source);
    write_file(
        &provider,
        "unit RequestedUnit;\ninterface\nprocedure RequestedRoutine;\nimplementation\nprocedure RequestedRoutine; begin end;\nend.\n",
    );
    let mut package_source = String::from("package RequestedPackage;\ncontains\n");
    package_source.push_str("  RequestedUnit in 'valid/RequestedUnit.pas'");
    for index in 0..1_024 {
        package_source.push_str(&format!(", RequestedUnit in 'missing/Unit{index:04}.pas'"));
    }
    package_source.push_str(";\nend.\n");
    write_file(&package, &package_source);
    write_file(
        &root.join("App.dproj"),
        "<Project><PropertyGroup><MainSource>Main.pas</MainSource><DCC_UsePackage>RequestedPackage</DCC_UsePackage></PropertyGroup></Project>",
    );

    let mut workspace = Workspace::new(vec![root], WorkspaceOptions::default());
    assert!(
        workspace
            .navigate(
                &uri(&main),
                position_of(main_source, "RequestedRoutine", 0),
                NavigationTarget::Declaration,
            )
            .is_empty()
    );
    assert!(
        workspace
            .warnings()
            .iter()
            .any(|warning| warning.contains("package unit candidate limit (1024)"))
    );
}

#[test]
fn oversized_package_metadata_is_skipped_without_loading_package_units() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path().join("workspace");
    let main = root.join("Main.pas");
    let package = root.join("Package.dpk");
    let main_source = "unit Main;\ninterface\nuses MissingUnit;\nimplementation\nprocedure Run;\nbegin\n  MissingRoutine;\nend;\nend.\n";
    write_file(&main, main_source);
    let mut oversized = String::from("package Package;\ncontains\n");
    oversized.push_str(&"X".repeat(4 * 1024 * 1024 + 1));
    write_file(&package, &oversized);
    write_file(
        &root.join("App.dproj"),
        "<Project><PropertyGroup><MainSource>Main.pas</MainSource><DCC_UsePackage>Package</DCC_UsePackage></PropertyGroup></Project>",
    );

    let mut workspace = Workspace::new(vec![root], WorkspaceOptions::default());
    assert!(
        workspace
            .navigate(
                &uri(&main),
                position_of(main_source, "MissingRoutine", 0),
                NavigationTarget::Declaration,
            )
            .is_empty()
    );
    assert_eq!(workspace.parsed_document_count(), 1);
    assert!(
        workspace
            .warnings()
            .iter()
            .any(|warning| warning.contains("safety limit"))
    );
}

#[test]
fn opening_a_project_does_not_parse_package_sources_until_navigation_needs_one() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path().join("workspace");
    let main = root.join("Main.pas");
    let provider = root.join("Provider.pas");
    let package = root.join("Package.dpk");
    let main_source = "unit Main;\ninterface\nuses Provider;\nimplementation\nend.\n";
    write_file(&main, main_source);
    write_file(
        &provider,
        "unit Provider;\ninterface\nprocedure ProviderRoutine;\nimplementation\nprocedure ProviderRoutine; begin end;\nend.\n",
    );
    write_file(
        &package,
        "package Package;\ncontains\n  Provider in 'Provider.pas';\nend.\n",
    );
    write_file(
        &root.join("App.dproj"),
        "<Project><PropertyGroup><MainSource>Main.pas</MainSource><DCC_UsePackage>Package</DCC_UsePackage></PropertyGroup></Project>",
    );

    let main_uri = uri(&main);
    let mut workspace = Workspace::new(vec![root], WorkspaceOptions::default());
    assert_eq!(workspace.parsed_document_count(), 0);
    workspace
        .open_document(main_uri, main_source.to_string(), 1)
        .expect("open main document");
    assert_eq!(workspace.parsed_document_count(), 1);
}

#[test]
fn dproj_without_a_package_main_source_is_not_a_package_by_filename_alone() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path().join("workspace");
    let main = root.join("Main.pas");
    let provider = root.join("packages/Provider.pas");
    let main_source = "unit Main;\ninterface\nuses Provider;\nimplementation\nprocedure Run;\nbegin\n  ProviderRoutine;\nend;\nend.\n";
    write_file(&main, main_source);
    write_file(
        &provider,
        "unit Provider;\ninterface\nprocedure ProviderRoutine;\nimplementation\nprocedure ProviderRoutine; begin end;\nend.\n",
    );
    write_file(
        &root.join("packages/Package.dproj"),
        "<Project><ItemGroup><DCCReference Include=\"Provider.pas\" /></ItemGroup></Project>",
    );
    write_file(
        &root.join("App.dproj"),
        "<Project><PropertyGroup><MainSource>Main.pas</MainSource><DCC_UsePackage>Package</DCC_UsePackage></PropertyGroup></Project>",
    );

    let mut workspace = Workspace::new(vec![root], WorkspaceOptions::default());
    assert!(
        workspace
            .navigate(
                &uri(&main),
                position_of(main_source, "ProviderRoutine", 0),
                NavigationTarget::Declaration,
            )
            .is_empty()
    );
}

#[test]
fn package_dproj_references_are_used_when_a_same_stem_dpk_is_unavailable() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path().join("workspace");
    let main = root.join("Main.pas");
    let provider = root.join("packages/src/Provider.pas");
    let package_project = root.join("packages/ProviderPackage.dproj");
    let main_source = "unit Main;\ninterface\nuses Provider;\nimplementation\nprocedure Run;\nbegin\n  ProviderRoutine;\nend;\nend.\n";
    write_file(&main, main_source);
    write_file(
        &provider,
        "unit Provider;\ninterface\nprocedure ProviderRoutine;\nimplementation\nprocedure ProviderRoutine; begin end;\nend.\n",
    );
    write_file(
        &package_project,
        "<Project><PropertyGroup><MainSource>ProviderPackage.dpk</MainSource></PropertyGroup><ItemGroup><DCCReference Include=\"src/Provider.pas\" /></ItemGroup></Project>",
    );
    write_file(
        &root.join("App.dproj"),
        "<Project><PropertyGroup><MainSource>Main.pas</MainSource><DCC_UsePackage>ProviderPackage</DCC_UsePackage></PropertyGroup></Project>",
    );

    let mut workspace = Workspace::new(vec![root], WorkspaceOptions::default());
    let locations = workspace.navigate(
        &uri(&main),
        position_of(main_source, "ProviderRoutine", 0),
        NavigationTarget::Declaration,
    );

    assert_eq!(locations.len(), 1);
    assert_eq!(locations[0].uri, uri(&provider));
}

#[test]
fn bounded_package_catalogue_finds_late_packages_and_rejects_late_duplicates() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path().join("workspace");
    let target_main = root.join("target-project/TargetMain.pas");
    let duplicate_main = root.join("duplicate-project/DuplicateMain.pas");
    let target_source = "unit TargetMain;\ninterface\nuses TargetUnit;\nimplementation\nprocedure Run;\nbegin\n  TargetRoutine;\nend;\nend.\n";
    let duplicate_source = "unit DuplicateMain;\ninterface\nuses SharedUnit;\nimplementation\nprocedure Run;\nbegin\n  SharedRoutine;\nend;\nend.\n";

    write_file(&target_main, target_source);
    write_file(
        &root.join("target-project/Target.dproj"),
        "<Project><PropertyGroup><MainSource>TargetMain.pas</MainSource><DCC_UsePackage>TargetPackage</DCC_UsePackage></PropertyGroup></Project>",
    );
    write_file(&duplicate_main, duplicate_source);
    write_file(
        &root.join("duplicate-project/Duplicate.dproj"),
        "<Project><PropertyGroup><MainSource>DuplicateMain.pas</MainSource><DCC_UsePackage>Shared</DCC_UsePackage></PropertyGroup></Project>",
    );

    for (directory, routine) in [("a-shared", "FirstRoutine"), ("z-shared", "SecondRoutine")] {
        write_file(
            &root.join(directory).join("Shared.dpk"),
            "package Shared;\ncontains\n  SharedUnit in 'SharedUnit.pas';\nend.\n",
        );
        write_file(
            &root.join(directory).join("SharedUnit.pas"),
            &format!(
                "unit SharedUnit;\ninterface\nprocedure {routine};\nimplementation\nprocedure {routine}; begin end;\nend.\n"
            ),
        );
    }

    let noise_root = root.join("b-large-metadata");
    fs::create_dir_all(&noise_root).expect("create large metadata directory");
    for index in 0..10_001 {
        write_file(
            &noise_root.join(format!("Noise{index:05}.dproj")),
            "<Project />",
        );
    }

    let target_package = root.join("z-target/TargetPackage.dpk");
    let target_provider = root.join("z-target/TargetUnit.pas");
    write_file(
        &target_package,
        "package TargetPackage;\ncontains\n  TargetUnit in 'TargetUnit.pas';\nend.\n",
    );
    write_file(
        &target_provider,
        "unit TargetUnit;\ninterface\nprocedure TargetRoutine;\nimplementation\nprocedure TargetRoutine; begin end;\nend.\n",
    );

    let mut target_workspace = Workspace::new(vec![root.clone()], WorkspaceOptions::default());
    let target_locations = target_workspace.navigate(
        &uri(&target_main),
        position_of(target_source, "TargetRoutine", 0),
        NavigationTarget::Declaration,
    );
    assert_eq!(target_locations.len(), 1);
    assert_eq!(target_locations[0].uri, uri(&target_provider));

    let mut duplicate_workspace = Workspace::new(vec![root], WorkspaceOptions::default());
    assert!(
        duplicate_workspace
            .navigate(
                &uri(&duplicate_main),
                position_of(duplicate_source, "SharedRoutine", 0),
                NavigationTarget::Declaration,
            )
            .is_empty()
    );
    assert!(
        duplicate_workspace
            .warnings()
            .iter()
            .any(|warning| warning.contains("ambiguous package shared"))
    );
}

#[test]
fn rename_capabilities_and_unopened_consumer_are_supported() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path().join("fixture");
    let provider = root.join("Provider.pas");
    let consumer = root.join("Consumer.pas");
    let provider_source =
        "unit Provider;\ninterface\nconst\n  kSQLDebugFile = 'debug.sql';\nimplementation\nend.\n";
    let consumer_source = "unit Consumer;\ninterface\nuses Provider;\nimplementation\nprocedure Use;\nbegin\n  Log(kSQLDebugFile);\nend;\nend.\n";
    write_file(&provider, provider_source);
    write_file(&consumer, consumer_source);

    let mut server = TestServer::launch();
    let initialize = server.initialize(&root, Value::Null);
    let capabilities = &initialize["capabilities"];
    assert_eq!(capabilities["renameProvider"]["prepareProvider"], true);
    assert_eq!(capabilities["codeActionProvider"]["resolveProvider"], true);
    assert!(
        capabilities["codeActionProvider"]["codeActionKinds"]
            .as_array()
            .expect("code action kinds")
            .iter()
            .any(|kind| kind == "quickfix")
    );

    let rename_id = RequestId::from("rename-unopened-consumer".to_string());
    server.send_request(
        rename_id.clone(),
        "textDocument/rename",
        json!({
            "textDocument": {"uri": uri(&provider)},
            "position": {"line": 3, "character": 14},
            "newName": "K_SQL_DEBUG_FILE"
        }),
    );
    let response = server.response(&rename_id);
    assert!(response.error.is_none(), "rename failed: {response:?}");
    let result = response.result.expect("rename result");
    assert!(
        result["documentChanges"]
            .as_array()
            .expect("document changes")
            .len()
            >= 2
    );
    assert_eq!(
        fs::read_to_string(&provider).expect("provider remains unchanged"),
        provider_source
    );
    assert_eq!(
        fs::read_to_string(&consumer).expect("consumer remains unchanged"),
        consumer_source
    );
    server.shutdown();
}

#[test]
fn unicode_shadow_recovery_refuses_explicit_eager_and_resolved_partial_edits() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path().join("fixture");
    let provider = root.join("Provider.pas");
    let consumer = root.join("Consumer.pas");
    let provider_source = "unit Provider;\ninterface\nconst badConst = 1;\nimplementation\nend.\n";
    let consumer_source = "unit Consumer;\ninterface\nuses Provider;\nimplementation\nprocedure Use;\nvar badConsté: Integer;\nbegin\n  badConsté := 2;\n  WriteLn(badConst);\n  WriteLn(badConsté);\nend;\nend.\n";
    write_file(
        &root.join(".lint4d.toml"),
        "[rules.naming]\nconstant_style = \"UPPER_CASE\"\n",
    );
    write_file(&provider, provider_source);
    write_file(&consumer, consumer_source);
    let selected = position_of(provider_source, "badConst", 0);
    let diagnostic_end = Position::new(selected.line, selected.character + 8);

    let mut server = TestServer::launch();
    server.initialize(&root, Value::Null);
    let rename_id = RequestId::from("unicode-shadow-explicit".to_string());
    server.send_request(
        rename_id.clone(),
        "textDocument/rename",
        json!({
            "textDocument": {"uri": uri(&provider)},
            "position": selected,
            "newName": "BAD_CONST"
        }),
    );
    let rename_response = server.response(&rename_id);
    assert!(
        rename_response.error.is_some(),
        "explicit rename must refuse the recovered Unicode shadow"
    );
    server.shutdown();

    let mut server = TestServer::launch();
    server.initialize(&root, Value::Null);
    let eager_id = RequestId::from("unicode-shadow-eager".to_string());
    server.send_request(
        eager_id.clone(),
        "textDocument/codeAction",
        json!({
            "textDocument": {"uri": uri(&provider)},
            "range": {"start": selected, "end": diagnostic_end},
            "context": {
                "diagnostics": [{
                    "range": {"start": selected, "end": diagnostic_end},
                    "severity": 4,
                    "code": "constant-naming",
                    "source": "lint4d",
                    "message": "naming violation"
                }],
                "only": ["quickfix"]
            }
        }),
    );
    let eager_response = server.response(&eager_id);
    assert!(
        eager_response.error.is_none(),
        "eager codeAction failed: {eager_response:?}"
    );
    assert_eq!(eager_response.result.expect("eager actions"), json!([]));
    server.shutdown();

    let mut server = TestServer::launch();
    server.initialize_with_action_support(&root, Value::Null);
    let actions_id = RequestId::from("unicode-shadow-resolved-actions".to_string());
    server.send_request(
        actions_id.clone(),
        "textDocument/codeAction",
        json!({
            "textDocument": {"uri": uri(&provider)},
            "range": {"start": selected, "end": diagnostic_end},
            "context": {
                "diagnostics": [{
                    "range": {"start": selected, "end": diagnostic_end},
                    "severity": 4,
                    "code": "constant-naming",
                    "source": "lint4d",
                    "message": "naming violation"
                }],
                "only": ["quickfix"]
            }
        }),
    );
    let actions_response = server.response(&actions_id);
    assert!(
        actions_response.error.is_none(),
        "resolved codeAction discovery failed: {actions_response:?}"
    );
    let actions = actions_response.result.expect("resolved actions");
    assert_eq!(actions.as_array().expect("action array").len(), 1);
    assert!(actions[0]["edit"].is_null());

    let resolve_id = RequestId::from("unicode-shadow-resolved".to_string());
    server.send_request(resolve_id.clone(), "codeAction/resolve", actions[0].clone());
    let resolved_response = server.response(&resolve_id);
    assert!(
        resolved_response.error.is_none(),
        "resolved codeAction request failed instead of returning a disabled action: {resolved_response:?}"
    );
    let resolved_action = resolved_response.result.expect("resolved action");
    assert!(resolved_action["edit"].is_null());
    assert!(
        resolved_action["disabled"]["reason"]
            .as_str()
            .expect("disabled reason")
            .contains("parser recovery")
    );
    server.shutdown();
}

#[test]
fn unopened_utf16le_and_utf16be_consumers_never_produce_partial_renames() {
    let provider_source = "unit Provider;\ninterface\nconst badConst = 1;\nimplementation\nend.\n";
    let consumer_source = "unit Consumer;\ninterface\nuses Provider;\nimplementation\nprocedure Use;\nbegin\n  WriteLn(badConst);\nend;\nend.\n";

    for (label, little_endian) in [("le", true), ("be", false)] {
        let temp = tempfile::tempdir().expect("temporary workspace");
        let root = temp.path().join("fixture");
        let provider = root.join("Provider.pas");
        let consumer = root.join("Consumer.pas");
        write_file(&provider, provider_source);

        let mut encoded = if little_endian {
            vec![0xFF, 0xFE]
        } else {
            vec![0xFE, 0xFF]
        };
        for code_unit in consumer_source.encode_utf16() {
            let bytes = if little_endian {
                code_unit.to_le_bytes()
            } else {
                code_unit.to_be_bytes()
            };
            encoded.extend_from_slice(&bytes);
        }
        fs::write(&consumer, encoded).expect("write UTF-16 consumer");

        let mut server = TestServer::launch();
        server.initialize(&root, Value::Null);
        let request_id = RequestId::from(format!("utf16-{label}-rename"));
        server.send_request(
            request_id.clone(),
            "textDocument/rename",
            json!({
                "textDocument": {"uri": uri(&provider)},
                "position": position_of(provider_source, "badConst", 0),
                "newName": "BAD_CONST"
            }),
        );
        let response = server.response(&request_id);
        assert!(
            response.error.is_some(),
            "UTF-16 {label} consumer must cause an explicit no-edit refusal: {response:?}"
        );
        server.shutdown();
    }
}

#[test]
fn prepare_rename_returns_the_original_utf16_identifier_range() {
    let (_temp, main, provider, _main_source, provider_source) = standard_workspace();
    let root = main.parent().expect("workspace root");
    let mut server = TestServer::launch();
    server.initialize(root, Value::Null);

    let request_id = RequestId::from("prepare-rename".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/prepareRename",
        json!({
            "textDocument": {"uri": uri(&provider)},
            "position": position_of(&provider_source, "PublicRoutine", 0)
        }),
    );
    let response = server.response(&request_id);
    assert!(
        response.error.is_none(),
        "prepareRename failed: {response:?}"
    );
    let result = response.result.expect("prepare result");
    assert_eq!(result["start"], json!({"line": 2, "character": 10}));
    assert_eq!(result["end"], json!({"line": 2, "character": 23}));
    server.shutdown();
}

#[test]
fn rename_returns_versioned_open_and_null_version_closed_document_edits() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path().join("fixture");
    let provider = root.join("Provider.pas");
    let consumer = root.join("Consumer.pas");
    let provider_source =
        "unit Provider;\ninterface\nconst\n  kSQLDebugFile = 'debug.sql';\nimplementation\nend.\n";
    let consumer_source = "unit Consumer;\ninterface\nuses Provider;\nimplementation\nprocedure Use;\nbegin\n  Log(kSQLDebugFile);\nend;\nend.\n";
    write_file(&provider, provider_source);
    write_file(&consumer, consumer_source);

    let mut server = TestServer::launch();
    server.initialize(&root, Value::Null);
    server.send_notification(
        "textDocument/didOpen",
        json!({"textDocument": {"uri": uri(&provider), "languageId": "pascal", "version": 7, "text": provider_source}}),
    );
    let _ = server.notification("textDocument/publishDiagnostics");

    let request_id = RequestId::from("versioned-rename".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/rename",
        json!({
            "textDocument": {"uri": uri(&provider)},
            "position": {"line": 3, "character": 14},
            "newName": "K_SQL_DEBUG_FILE"
        }),
    );
    let response = server.response(&request_id);
    assert!(response.error.is_none(), "rename failed: {response:?}");
    let result = response.result.expect("rename result");
    let changes = result["documentChanges"]
        .as_array()
        .expect("document changes");
    let provider_change = changes
        .iter()
        .find(|change| change["textDocument"]["uri"] == uri(&provider).to_string())
        .expect("open provider edit");
    assert_eq!(provider_change["textDocument"]["version"], 7);
    let consumer_change = changes
        .iter()
        .find(|change| change["textDocument"]["uri"] == uri(&consumer).to_string())
        .expect("closed consumer edit");
    assert!(consumer_change["textDocument"]["version"].is_null());
    assert_eq!(
        fs::read_to_string(&provider).expect("provider remains unchanged"),
        provider_source
    );
    assert_eq!(
        fs::read_to_string(&consumer).expect("consumer remains unchanged"),
        consumer_source
    );
    server.shutdown();
}

#[test]
fn code_action_revalidates_a_single_constant_diagnostic_and_eagerly_shares_rename_edits() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path().join("fixture");
    let main = root.join("Main.pas");
    let source = "unit Main;\ninterface\nconst\n  badConst = 1;\nimplementation\nprocedure Use;\nbegin\n  Log(badConst);\nend;\nend.\n";
    write_file(
        &root.join(".lint4d.toml"),
        "[rules.naming]\nconstant_style = \"UPPER_CASE\"\n",
    );
    write_file(&main, source);

    let diagnostic_start = position_of(source, "badConst", 0);
    let diagnostic_end = Position::new(diagnostic_start.line, diagnostic_start.character + 8);
    let mut server = TestServer::launch();
    server.initialize(&root, Value::Null);
    let request_id = RequestId::from("constant-code-action".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/codeAction",
        json!({
            "textDocument": {"uri": uri(&main)},
            "range": {"start": diagnostic_start, "end": diagnostic_end},
            "context": {
                "diagnostics": [{
                    "range": {"start": diagnostic_start, "end": diagnostic_end},
                    "severity": 4,
                    "code": "constant-naming",
                    "source": "lint4d",
                    "message": "client text is intentionally not trusted"
                }],
                "only": ["quickfix"]
            }
        }),
    );
    let response = server.response(&request_id);
    assert!(response.error.is_none(), "codeAction failed: {response:?}");
    let actions = response.result.expect("code action result");
    assert_eq!(actions.as_array().expect("actions").len(), 1);
    let action = &actions[0];
    assert_eq!(action["title"], "Rename 'badConst' to 'BAD_CONST'");
    assert_eq!(action["kind"], "quickfix");
    assert!(
        action["edit"].is_object(),
        "old clients receive eager edits: {action}"
    );
    assert!(
        action["data"].is_object(),
        "action identity is opaque and bounded: {action}"
    );

    let rename_id = RequestId::from("constant-code-action-equivalence".to_string());
    server.send_request(
        rename_id.clone(),
        "textDocument/rename",
        json!({
            "textDocument": {"uri": uri(&main)},
            "position": diagnostic_start,
            "newName": "BAD_CONST"
        }),
    );
    let rename_response = server.response(&rename_id);
    assert!(
        rename_response.error.is_none(),
        "explicit rename failed: {rename_response:?}"
    );
    assert_eq!(
        action["edit"],
        rename_response.result.expect("explicit rename result"),
        "naming quick-fix and explicit rename must share the edit planner"
    );
    server.shutdown();
}

#[test]
fn code_action_resolve_rechecks_identity_and_rejects_stale_source_actions() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path().join("fixture");
    let main = root.join("Main.pas");
    let source = "unit Main;\ninterface\nconst\n  badConst = 1;\nimplementation\nprocedure Use;\nbegin\n  Log(badConst);\nend;\nend.\n";
    write_file(
        &root.join(".lint4d.toml"),
        "[rules.naming]\nconstant_style = \"UPPER_CASE\"\n",
    );
    write_file(&main, source);
    let start = position_of(source, "badConst", 0);
    let end = Position::new(start.line, start.character + 8);

    let mut server = TestServer::launch();
    server.initialize_with_action_support(&root, Value::Null);
    let action_id = RequestId::from("unresolved-constant-action".to_string());
    server.send_request(
        action_id.clone(),
        "textDocument/codeAction",
        json!({
            "textDocument": {"uri": uri(&main)},
            "range": {"start": start, "end": end},
            "context": {
                "diagnostics": [{
                    "range": {"start": start, "end": end},
                    "severity": 4,
                    "code": "constant-naming",
                    "source": "lint4d",
                    "message": "ignored by the server"
                }],
                "only": ["quickfix"]
            }
        }),
    );
    let action_response = server.response(&action_id);
    assert!(
        action_response.error.is_none(),
        "codeAction failed: {action_response:?}"
    );
    let action = action_response.result.expect("actions")[0].clone();
    assert!(action["edit"].is_null());
    assert!(action["data"].is_object());
    for field in [
        "sourceGeneration",
        "configurationGeneration",
        "configFingerprint",
        "sourceHash",
    ] {
        assert!(
            action["data"][field].is_string(),
            "code-action {field} must survive JavaScript/Lua number round trips: {action}"
        );
    }

    let resolve_id = RequestId::from("resolve-constant-action".to_string());
    server.send_request(resolve_id.clone(), "codeAction/resolve", action.clone());
    let resolved = server.response(&resolve_id);
    assert!(resolved.error.is_none(), "resolve failed: {resolved:?}");
    let resolved_action = resolved.result.expect("resolved action");
    assert!(resolved_action["edit"].is_object());
    assert_eq!(resolved_action["title"], action["title"]);
    assert_eq!(resolved_action["data"], action["data"]);

    let mut tampered = action.clone();
    tampered["data"]["newName"] = json!("EVIL_NAME");
    let tampered_id = RequestId::from("tampered-constant-action".to_string());
    server.send_request(tampered_id.clone(), "codeAction/resolve", tampered);
    assert!(server.response(&tampered_id).error.is_some());

    server.send_notification(
        "textDocument/didOpen",
        json!({"textDocument": {"uri": uri(&main), "languageId": "pascal", "version": 1, "text": source}}),
    );
    let _ = server.notification("textDocument/publishDiagnostics");
    let stale_id = RequestId::from("stale-constant-action".to_string());
    server.send_request(stale_id.clone(), "codeAction/resolve", action);
    let stale = server.response(&stale_id);
    assert!(
        stale.error.is_some(),
        "stale resolve must not produce edits: {stale:?}"
    );
    server.shutdown();
}

#[test]
fn code_action_resolve_rejects_closed_source_changes_without_a_generation_event() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path().join("fixture");
    let main = root.join("Main.pas");
    let source = "unit Main;\ninterface\nconst\n  badConst = 1;\nimplementation\nprocedure Use;\nbegin\n  Log(badConst);\nend;\nend.\n";
    write_file(
        &root.join(".lint4d.toml"),
        "[rules.naming]\nconstant_style = \"UPPER_CASE\"\n",
    );
    write_file(&main, source);
    let start = position_of(source, "badConst", 0);
    let end = Position::new(start.line, start.character + 8);

    let mut server = TestServer::launch();
    server.initialize_with_action_support(&root, Value::Null);
    let action_id = RequestId::from("closed-source-action".to_string());
    server.send_request(
        action_id.clone(),
        "textDocument/codeAction",
        json!({
            "textDocument": {"uri": uri(&main)},
            "range": {"start": start, "end": end},
            "context": {
                "diagnostics": [{
                    "range": {"start": start, "end": end},
                    "severity": 4,
                    "code": "constant-naming",
                    "source": "lint4d",
                    "message": "naming violation"
                }],
                "only": ["quickfix"]
            }
        }),
    );
    let action_response = server.response(&action_id);
    assert!(
        action_response.error.is_none(),
        "codeAction failed: {action_response:?}"
    );
    let action = action_response.result.expect("actions")[0].clone();
    assert!(action["edit"].is_null());

    write_file(
        &main,
        &source.replace("Log(badConst)", "Log(badConst);\n  Log(1)"),
    );
    let resolve_id = RequestId::from("closed-source-action-resolve".to_string());
    server.send_request(resolve_id.clone(), "codeAction/resolve", action);
    let resolved = server.response(&resolve_id);
    assert!(
        resolved.error.is_some(),
        "resolve must reject a changed closed source without a generation event: {resolved:?}"
    );
    server.shutdown();
}

#[test]
fn code_action_offers_local_variable_fix_using_the_configured_conversion() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path().join("fixture");
    let main = root.join("Main.pas");
    let source = "unit Main;\ninterface\nimplementation\nprocedure Use;\nvar\n  BadVariable: Integer;\nbegin\n  BadVariable := 1;\nend;\nend.\n";
    write_file(
        &root.join(".lint4d.toml"),
        "[rules.naming]\nlocal_variable_style = \"camelCase\"\n",
    );
    write_file(&main, source);
    let start = position_of(source, "BadVariable", 0);
    let end = Position::new(start.line, start.character + 11);

    let mut server = TestServer::launch();
    server.initialize(&root, Value::Null);
    let request_id = RequestId::from("local-code-action".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/codeAction",
        json!({
            "textDocument": {"uri": uri(&main)},
            "range": {"start": start, "end": end},
            "context": {
                "diagnostics": [{
                    "range": {"start": start, "end": end},
                    "severity": 4,
                    "code": "local-variable-naming",
                    "message": "old client wording"
                }],
                "only": ["quickfix"]
            }
        }),
    );
    let response = server.response(&request_id);
    assert!(
        response.error.is_none(),
        "local codeAction failed: {response:?}"
    );
    let actions = response.result.expect("local actions");
    assert_eq!(actions.as_array().expect("actions").len(), 1);
    assert_eq!(actions[0]["title"], "Rename 'BadVariable' to 'badVariable'");
    assert!(actions[0]["edit"].is_object());
    server.shutdown();
}

#[test]
fn code_actions_honor_requested_kind_filter() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path().join("fixture");
    let main = root.join("Main.pas");
    let source = "unit Main;\ninterface\nconst\n  badConst = 1;\nimplementation\nend.\n";
    write_file(
        &root.join(".lint4d.toml"),
        "[rules.naming]\nconstant_style = \"UPPER_CASE\"\n",
    );
    write_file(&main, source);
    let start = position_of(source, "badConst", 0);
    let end = Position::new(start.line, start.character + 8);

    let mut server = TestServer::launch();
    server.initialize(&root, Value::Null);
    let request_id = RequestId::from("refactor-only-code-actions".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/codeAction",
        json!({
            "textDocument": {"uri": uri(&main)},
            "range": {"start": start, "end": end},
            "context": {
                "diagnostics": [],
                "only": ["refactor"]
            }
        }),
    );
    let response = server.response(&request_id);
    assert!(response.error.is_none(), "codeAction failed: {response:?}");
    assert_eq!(response.result.expect("code actions"), json!([]));
    server.shutdown();
}

#[test]
fn rename_rejects_or_returns_the_complete_edit_set_after_dependency_eviction() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path().join("fixture");
    let external = temp.path().join("external");
    let provider = root.join("Provider.pas");
    let consumer = root.join("Consumer.pas");
    let d = root.join("D.pas");
    let a = root.join("A.pas");
    let provider_source =
        "unit Provider;\ninterface\nconst\n  badConst = 1;\nimplementation\nend.\n";
    let consumer_source = "unit Consumer;\ninterface\nuses Provider;\nimplementation\nprocedure Use;\nbegin\n  Log(badConst);\nend;\nend.\n";
    let d_source = "unit D;\ninterface\nuses Provider;\nimplementation\nprocedure Use;\nbegin\n  Log(badConst);\nend;\nend.\n";
    let a_source = "unit A;\ninterface\nuses Extra;\nimplementation\nend.\n";
    write_file(&provider, provider_source);
    write_file(&consumer, consumer_source);
    write_file(&d, d_source);
    write_file(&a, a_source);
    write_file(
        &root.join("App.dproj"),
        "<Project><PropertyGroup><DCC_UnitSearchPath>../external</DCC_UnitSearchPath></PropertyGroup></Project>",
    );
    write_file(
        &external.join("Extra.pas"),
        "unit Extra;\ninterface\nimplementation\nend.\n",
    );

    let expected: HashSet<String> = [uri(&provider), uri(&consumer), uri(&d)]
        .into_iter()
        .map(|uri| uri.to_string())
        .collect();
    for attempt in 0..5 {
        let mut server = TestServer::launch();
        server.initialize(&root, json!({"maxFiles": 4}));
        let request_id = RequestId::from(format!("dependency-eviction-{attempt}"));
        server.send_request(
            request_id.clone(),
            "textDocument/rename",
            json!({
                "textDocument": {"uri": uri(&provider)},
                "position": position_of(provider_source, "badConst", 0),
                "newName": "BAD_CONST"
            }),
        );
        let response = server.response(&request_id);
        if let Some(error) = response.error {
            assert_eq!(
                error.code, -32803,
                "unexpected dependency failure: {error:?}"
            );
        } else {
            let edit = response.result.expect("rename result");
            assert_eq!(
                workspace_edit_uris(&edit),
                expected,
                "a successful rename must include every reverse consumer"
            );
        }
        server.shutdown();
    }
}

#[test]
fn local_rename_does_not_use_the_workspace_retained_file_cap_for_unrelated_sources() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path().join("fixture");
    let main = root.join("Main.pas");
    let source = "unit Main;\ninterface\nimplementation\nprocedure Run;\nvar\n  BadVariable: Integer;\nbegin\n  BadVariable := 1;\nend;\nend.\n";
    write_file(&main, source);
    for index in 0..40 {
        write_file(
            &root.join(format!("Noise{index:02}.pas")),
            &format!("unit Noise{index:02};\ninterface\nimplementation\nend.\n"),
        );
    }
    write_file(&root.join("Malformed.pas"), "not a Pascal source; ???\n");

    let mut server = TestServer::launch();
    server.initialize(&root, json!({"maxFiles": 1}));
    let request_id = RequestId::from("local-rename-retained-cap".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/rename",
        json!({
            "textDocument": {"uri": uri(&main)},
            "position": position_of(source, "BadVariable", 0),
            "newName": "badVariable"
        }),
    );
    let response = server.response(&request_id);
    assert!(
        response.error.is_none(),
        "a local rename must not scan unrelated sources into its retained budget: {response:?}"
    );
    assert_eq!(
        workspace_edit_uris(&response.result.expect("local rename result")),
        HashSet::from([uri(&main).to_string()])
    );
    server.shutdown();
}

#[test]
fn local_rename_ignores_unresolved_imports_when_binding_is_provably_local() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path().join("fixture");
    let main = root.join("Main.pas");
    let source = "unit Main;\ninterface\nuses MissingSdkUnit;\nimplementation\nprocedure Run;\nvar\n  LocalValue: Integer;\nbegin\n  LocalValue := 1;\nend;\nend.\n";
    write_file(&main, source);

    let mut server = TestServer::launch();
    server.initialize(&root, Value::Null);
    let request_id = RequestId::from("local-rename-missing-import".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/rename",
        json!({
            "textDocument": {"uri": uri(&main)},
            "position": position_of(source, "LocalValue", 0),
            "newName": "renamedLocalValue"
        }),
    );
    let response = server.response(&request_id);
    assert!(
        response.error.is_none(),
        "a proven local rename must not depend on unrelated imports: {response:?}"
    );
    assert_eq!(
        workspace_edit_uris(&response.result.expect("local rename result")),
        HashSet::from([uri(&main).to_string()])
    );
    server.shutdown();
}

#[test]
fn public_rename_ignores_unresolved_imports_when_binding_is_provably_self_contained() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path().join("fixture");
    let provider = root.join("Provider.pas");
    let provider_source = "unit Provider;\ninterface\nuses MissingSdkUnit;\ntype\n  TLog = class\n  public\n    const kSQLDebugFile = 'debug.sql';\n  end;\nimplementation\nprocedure Use;\nbegin\n  Log(TLog.kSQLDebugFile);\nend;\nend.\n";
    write_file(&provider, provider_source);
    write_file(
        &root.join("App.dproj"),
        "<Project><PropertyGroup><MainSource>Provider.pas</MainSource></PropertyGroup></Project>",
    );

    let mut server = TestServer::launch();
    server.initialize(&root, json!({"projectFile": "App.dproj"}));
    let request_id = RequestId::from("self-contained-public-rename".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/rename",
        json!({
            "textDocument": {"uri": uri(&provider)},
            "position": position_of(provider_source, "kSQLDebugFile", 0),
            "newName": "K_SQL_DEBUG_FILE"
        }),
    );
    let response = server.response(&request_id);
    assert!(
        response.error.is_none(),
        "same-document public binding must not depend on an unresolved SDK import: {response:?}"
    );
    assert_eq!(
        response
            .result
            .as_ref()
            .expect("self-contained rename result")["documentChanges"]
            .as_array()
            .expect("document changes")
            .iter()
            .map(|change| change["edits"].as_array().expect("document edits").len())
            .sum::<usize>(),
        2,
        "the declaration and same-document reference must both be edited"
    );
    assert_exact_rename_edits(
        response
            .result
            .as_ref()
            .expect("self-contained rename result"),
        &provider,
        provider_source,
        "kSQLDebugFile",
        2,
        "K_SQL_DEBUG_FILE",
    );
    assert_eq!(
        workspace_edit_uris(&response.result.expect("self-contained rename result")),
        HashSet::from([uri(&provider).to_string()])
    );
    server.shutdown();
}

#[test]
fn public_rename_rejects_self_contained_global_fallback_inside_unknown_ancestor_method() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path().join("fixture");
    let provider = root.join("Provider.pas");
    let provider_source = "unit Provider;\ninterface\ntype\n  TChild = class(TUnknownAncestor)\n  public\n    procedure Use;\n  end;\nconst\n  badConst = 1;\nimplementation\nprocedure TChild.Use;\nbegin\n  Log(badConst);\nend;\nend.\n";
    write_file(&provider, provider_source);
    write_file(
        &root.join("App.dproj"),
        "<Project><PropertyGroup><MainSource>Provider.pas</MainSource></PropertyGroup></Project>",
    );

    let mut server = TestServer::launch();
    server.initialize(&root, json!({"projectFile": "App.dproj"}));
    let request_id = RequestId::from("unknown-ancestor-global-fallback-rename".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/rename",
        json!({
            "textDocument": {"uri": uri(&provider)},
            "position": position_of(provider_source, "badConst", 0),
            "newName": "BAD_CONST"
        }),
    );
    let response = server.response(&request_id);
    assert!(
        response.error.is_some(),
        "a global fallback inside a method with an unknown ancestor is not self-contained: {response:?}"
    );
    server.shutdown();
}

#[test]
fn public_rename_rejects_global_fallback_through_unknown_local_intermediate_base() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path().join("fixture");
    let provider = root.join("Provider.pas");
    let provider_source = "unit Provider;\ninterface\nuses MissingSdk;\nconst badConst = 1;\ntype TLocalBase = class(TUnknownAncestor) end;\nTChild = class(TLocalBase)\n  procedure Use;\nend;\nimplementation\nprocedure TChild.Use;\nbegin\n  Log(badConst);\nend;\nend.\n";
    write_file(&provider, provider_source);
    write_file(
        &root.join("App.dproj"),
        "<Project><PropertyGroup><MainSource>Provider.pas</MainSource></PropertyGroup></Project>",
    );

    let mut server = TestServer::launch();
    server.initialize(&root, json!({"projectFile": "App.dproj"}));
    let request_id = RequestId::from("unknown-local-intermediate-base-rename".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/rename",
        json!({
            "textDocument": {"uri": uri(&provider)},
            "position": position_of(provider_source, "badConst", 0),
            "newName": "GOOD_CONST"
        }),
    );
    let response = server.response(&request_id);
    assert!(
        response.error.is_some(),
        "an unknown grandparent must not make a global fallback self-contained: {response:?}"
    );
    assert!(
        response.result.is_none(),
        "an unsafe intermediate-base rename must not return partial edits: {response:?}"
    );
    server.shutdown();
}

#[test]
fn public_rename_rejects_global_fallback_through_local_alias_to_unknown_base() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path().join("fixture");
    let provider = root.join("Provider.pas");
    let provider_source = "unit Provider;\ninterface\nuses MissingSdk;\nconst badConst = 1;\ntype TLocalBase = TUnknownAncestor;\nTChild = class(TLocalBase)\n  procedure Use;\nend;\nimplementation\nprocedure TChild.Use;\nbegin\n  Log(badConst);\nend;\nend.\n";
    write_file(&provider, provider_source);
    write_file(
        &root.join("App.dproj"),
        "<Project><PropertyGroup><MainSource>Provider.pas</MainSource></PropertyGroup></Project>",
    );

    let mut server = TestServer::launch();
    server.initialize(&root, json!({"projectFile": "App.dproj"}));
    let request_id = RequestId::from("unknown-local-alias-rename".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/rename",
        json!({
            "textDocument": {"uri": uri(&provider)},
            "position": position_of(provider_source, "badConst", 0),
            "newName": "GOOD_CONST"
        }),
    );
    let response = server.response(&request_id);
    assert!(
        response.error.is_some(),
        "an unknown aliased grandparent must not make a global fallback self-contained: {response:?}"
    );
    assert!(
        response.result.is_none(),
        "an unsafe alias rename must not return partial edits: {response:?}"
    );
    server.shutdown();
}

#[test]
fn public_rename_allows_same_class_member_over_unknown_ancestor_fallback() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path().join("fixture");
    let provider = root.join("Provider.pas");
    let provider_source = "unit Provider;\ninterface\ntype\n  TChild = class(TUnknownAncestor)\n  public\n    const badConst = 1;\n    procedure Use;\n  end;\nimplementation\nprocedure TChild.Use;\nbegin\n  Log(badConst);\nend;\nend.\n";
    write_file(&provider, provider_source);
    write_file(
        &root.join("App.dproj"),
        "<Project><PropertyGroup><MainSource>Provider.pas</MainSource></PropertyGroup></Project>",
    );

    let mut server = TestServer::launch();
    server.initialize(&root, json!({"projectFile": "App.dproj"}));
    let request_id = RequestId::from("same-class-member-unknown-ancestor-rename".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/rename",
        json!({
            "textDocument": {"uri": uri(&provider)},
            "position": position_of(provider_source, "badConst", 0),
            "newName": "BAD_CONST"
        }),
    );
    let response = server.response(&request_id);
    assert!(
        response.error.is_none(),
        "a declared same-class member outranks an unknown ancestor: {response:?}"
    );
    assert_exact_rename_edits(
        response
            .result
            .as_ref()
            .expect("same-class member rename result"),
        &provider,
        provider_source,
        "badConst",
        2,
        "BAD_CONST",
    );
    assert_eq!(
        workspace_edit_uris(&response.result.expect("same-class member rename result")),
        HashSet::from([uri(&provider).to_string()])
    );
    server.shutdown();
}

#[test]
fn public_rename_allows_local_parameter_over_unknown_ancestor_fallback() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path().join("fixture");
    let provider = root.join("Provider.pas");
    let provider_source = "unit Provider;\ninterface\ntype\n  TChild = class(TUnknownAncestor)\n  public\n    procedure Use(badConst: Integer);\n  end;\nimplementation\nprocedure TChild.Use(badConst: Integer);\nbegin\n  Log(badConst);\nend;\nend.\n";
    write_file(&provider, provider_source);
    write_file(
        &root.join("App.dproj"),
        "<Project><PropertyGroup><MainSource>Provider.pas</MainSource></PropertyGroup></Project>",
    );

    let mut server = TestServer::launch();
    server.initialize(&root, json!({"projectFile": "App.dproj"}));
    let request_id = RequestId::from("local-parameter-unknown-ancestor-rename".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/rename",
        json!({
            "textDocument": {"uri": uri(&provider)},
            "position": position_of(provider_source, "badConst", 1),
            "newName": "renamedParam"
        }),
    );
    let response = server.response(&request_id);
    assert!(
        response.error.is_none(),
        "a local parameter outranks an unknown ancestor: {response:?}"
    );
    assert_exact_rename_edits(
        response
            .result
            .as_ref()
            .expect("local parameter rename result"),
        &provider,
        provider_source,
        "badConst",
        3,
        "renamedParam",
    );
    assert_eq!(
        workspace_edit_uris(&response.result.expect("local parameter rename result")),
        HashSet::from([uri(&provider).to_string()])
    );
    server.shutdown();
}

#[test]
fn public_rename_refuses_an_imported_consumer_with_a_missing_potential_shadow() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path().join("fixture");
    let provider = root.join("Provider.pas");
    let consumer = root.join("Consumer.pas");
    let provider_source = "unit Provider;\ninterface\nuses MissingProviderSdk;\ntype\n  TLog = class\n  public\n    const kSQLDebugFile = 'debug.sql';\n  end;\nimplementation\nprocedure Use;\nbegin\n  Log(TLog.kSQLDebugFile);\nend;\nend.\n";
    let consumer_source = "unit Consumer;\ninterface\nuses Provider, MissingShadowUnit;\nimplementation\nprocedure Use;\nbegin\n  Log(TLog.kSQLDebugFile);\nend;\nend.\n";
    write_file(&provider, provider_source);
    write_file(&consumer, consumer_source);
    write_file(
        &root.join("App.dproj"),
        "<Project><PropertyGroup><MainSource>Provider.pas</MainSource></PropertyGroup></Project>",
    );

    let mut server = TestServer::launch();
    server.initialize(&root, json!({"projectFile": "App.dproj"}));
    let request_id = RequestId::from("missing-consumer-shadow-rename".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/rename",
        json!({
            "textDocument": {"uri": uri(&provider)},
            "position": position_of(provider_source, "kSQLDebugFile", 0),
            "newName": "K_SQL_DEBUG_FILE"
        }),
    );
    let response = server.response(&request_id);
    let error = response
        .error
        .expect("a missing potentially shadowing consumer import must refuse rename");
    assert!(
        error.message.to_ascii_lowercase().contains("incomplete")
            || error.message.to_ascii_lowercase().contains("import"),
        "unexpected missing-consumer error: {error:?}"
    );
    server.shutdown();
}

#[test]
fn public_rename_refuses_an_unknown_receiver_in_a_candidate_reference() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path().join("fixture");
    let provider = root.join("Provider.pas");
    let provider_source = "unit Provider;\ninterface\nuses MissingSdkUnit;\ntype\n  TLog = class\n  public\n    const kSQLDebugFile = 'debug.sql';\n  end;\nimplementation\nprocedure Use;\nbegin\n  Log(UnknownReceiver.kSQLDebugFile);\nend;\nend.\n";
    write_file(&provider, provider_source);
    write_file(
        &root.join("App.dproj"),
        "<Project><PropertyGroup><MainSource>Provider.pas</MainSource></PropertyGroup></Project>",
    );

    let mut server = TestServer::launch();
    server.initialize(&root, json!({"projectFile": "App.dproj"}));
    let request_id = RequestId::from("unknown-receiver-rename".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/rename",
        json!({
            "textDocument": {"uri": uri(&provider)},
            "position": position_of(provider_source, "kSQLDebugFile", 0),
            "newName": "K_SQL_DEBUG_FILE"
        }),
    );
    let response = server.response(&request_id);
    assert!(
        response.error.is_some(),
        "an unknown receiver must not authorize a partial public rename"
    );
    server.shutdown();
}

#[test]
fn public_rename_refuses_a_proposed_name_reference_through_an_import() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path().join("fixture");
    let provider = root.join("Provider.pas");
    let other = root.join("OtherUnit.pas");
    let provider_source = "unit Provider;\ninterface\nuses MissingSdkUnit, OtherUnit;\ntype\n  TLog = class\n  public\n    const kSQLDebugFile = 'debug.sql';\n  end;\nimplementation\nprocedure Use;\nbegin\n  Log(TLog.kSQLDebugFile);\n  Log(OtherUnit.K_SQL_DEBUG_FILE);\nend;\nend.\n";
    let other_source = "unit OtherUnit;\ninterface\nconst\n  K_SQL_DEBUG_FILE = 'other.sql';\nimplementation\nend.\n";
    write_file(&provider, provider_source);
    write_file(&other, other_source);
    write_file(
        &root.join("App.dproj"),
        "<Project><PropertyGroup><MainSource>Provider.pas</MainSource></PropertyGroup></Project>",
    );

    let mut server = TestServer::launch();
    server.initialize(&root, json!({"projectFile": "App.dproj"}));
    let request_id = RequestId::from("proposed-import-reference-rename".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/rename",
        json!({
            "textDocument": {"uri": uri(&provider)},
            "position": position_of(provider_source, "kSQLDebugFile", 0),
            "newName": "K_SQL_DEBUG_FILE"
        }),
    );
    let response = server.response(&request_id);
    assert!(
        response.error.is_some(),
        "a proposed name already used through an import must refuse rename"
    );
    server.shutdown();
}

#[test]
fn public_rename_does_not_silently_omit_include_only_consumers() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path().join("fixture");
    let provider = root.join("Provider.pas");
    let consumer = root.join("Consumer.pas");
    let body = root.join("Body.inc");
    let provider_source =
        "unit Provider;\ninterface\nconst\n  badConst = 1;\nimplementation\nend.\n";
    let consumer_source = "unit Consumer;\ninterface\nimplementation\n{$I Body.inc}\nend.\n";
    write_file(&provider, provider_source);
    write_file(&consumer, consumer_source);
    write_file(&body, "procedure Use;\nbegin\n  Log(badConst);\nend;\n");

    let mut server = TestServer::launch();
    server.initialize(&root, Value::Null);
    let request_id = RequestId::from("include-only-consumer-rename".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/rename",
        json!({
            "textDocument": {"uri": uri(&provider)},
            "position": position_of(provider_source, "badConst", 0),
            "newName": "BAD_CONST"
        }),
    );
    let response = server.response(&request_id);
    let error = response
        .error
        .expect("an unsupported source-bearing include must not be omitted");
    assert!(
        error.message.to_ascii_lowercase().contains("include"),
        "unexpected include error: {}",
        error.message
    );
    server.shutdown();
}

#[test]
fn public_rename_allows_an_irrelevant_source_bearing_include() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path().join("fixture");
    let provider = root.join("Provider.pas");
    let consumer = root.join("Consumer.pas");
    let body = root.join("Body.inc");
    let provider_source =
        "unit Provider;\ninterface\nconst\n  badConst = 1;\nimplementation\nend.\n";
    let consumer_source =
        "unit Consumer;\ninterface\nuses Provider;\nimplementation\n{$I Body.inc}\nend.\n";
    write_file(&provider, provider_source);
    write_file(&consumer, consumer_source);
    write_file(&body, "procedure Unrelated;\nbegin\nend;\n");

    let mut server = TestServer::launch();
    server.initialize(&root, Value::Null);
    let request_id = RequestId::from("irrelevant-source-include-rename".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/rename",
        json!({
            "textDocument": {"uri": uri(&provider)},
            "position": position_of(provider_source, "badConst", 0),
            "newName": "BAD_CONST"
        }),
    );
    let response = server.response(&request_id);
    assert!(
        response.error.is_none(),
        "an irrelevant source-bearing include must not block the rename: {response:?}"
    );
    assert_eq!(
        workspace_edit_uris(&response.result.expect("rename result")),
        HashSet::from([uri(&provider).to_string()])
    );
    server.shutdown();
}

#[test]
fn public_rename_rejects_an_unresolved_include_in_an_unrelated_source() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path().join("fixture");
    let provider = root.join("Provider.pas");
    let unrelated = root.join("Unrelated.pas");
    let provider_source =
        "unit Provider;\ninterface\nconst\n  badConst = 1;\nimplementation\nend.\n";
    let unrelated_source =
        "unit Unrelated;\ninterface\n{$I MissingUnrelated.inc}\nimplementation\nend.\n";
    write_file(&provider, provider_source);
    write_file(&unrelated, unrelated_source);

    let mut server = TestServer::launch();
    server.initialize(&root, Value::Null);
    let request_id = RequestId::from("unrelated-unresolved-include-rename".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/rename",
        json!({
            "textDocument": {"uri": uri(&provider)},
            "position": position_of(provider_source, "badConst", 0),
            "newName": "BAD_CONST"
        }),
    );
    let response = server.response(&request_id);
    let error = response
        .error
        .expect("an unresolved include must block rename even without uses/target tokens");
    assert!(
        error.message.to_ascii_lowercase().contains("include"),
        "unexpected unresolved unrelated include error: {error:?}"
    );
    server.shutdown();
}

#[test]
fn public_rename_audits_nested_source_bearing_include_before_allowing_irrelevant_content() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path().join("fixture");
    let provider = root.join("Provider.pas");
    let consumer = root.join("Consumer.pas");
    let parent = root.join("Parent.inc");
    let nested = root.join("Nested.inc");
    let provider_source =
        "unit Provider;\ninterface\nconst\n  badConst = 1;\nimplementation\nend.\n";
    let consumer_source = "unit Consumer;\ninterface\nimplementation\n{$I Parent.inc}\nend.\n";
    let parent_source = "procedure Irrelevant;\nbegin\nend;\n{$I Nested.inc}\n";
    write_file(&provider, provider_source);
    write_file(&consumer, consumer_source);
    write_file(&parent, parent_source);
    write_file(&nested, "{$DEFINE SAFE}\n");

    let mut server = TestServer::launch();
    server.initialize(&root, Value::Null);
    let request_id = RequestId::from("nested-safe-source-include-rename".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/rename",
        json!({
            "textDocument": {"uri": uri(&provider)},
            "position": position_of(provider_source, "badConst", 0),
            "newName": "BAD_CONST"
        }),
    );
    let response = server.response(&request_id);
    assert!(
        response.error.is_none(),
        "a fully audited irrelevant nested include must allow rename: {response:?}"
    );
    assert_eq!(
        workspace_edit_uris(&response.result.expect("rename result")),
        HashSet::from([uri(&provider).to_string()])
    );
    server.shutdown();
}

#[test]
fn public_rename_rejects_a_nested_candidate_in_a_source_bearing_include() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path().join("fixture");
    let provider = root.join("Provider.pas");
    let consumer = root.join("Consumer.pas");
    let parent = root.join("Parent.inc");
    let nested = root.join("Nested.inc");
    let provider_source =
        "unit Provider;\ninterface\nconst\n  badConst = 1;\nimplementation\nend.\n";
    let consumer_source = "unit Consumer;\ninterface\nimplementation\n{$I Parent.inc}\nend.\n";
    let parent_source = "procedure Irrelevant;\nbegin\nend;\n{$I Nested.inc}\n";
    write_file(&provider, provider_source);
    write_file(&consumer, consumer_source);
    write_file(&parent, parent_source);
    write_file(&nested, "procedure Use;\nbegin\n  Log(badConst);\nend;\n");

    let mut server = TestServer::launch();
    server.initialize(&root, Value::Null);
    let request_id = RequestId::from("nested-candidate-source-include-rename".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/rename",
        json!({
            "textDocument": {"uri": uri(&provider)},
            "position": position_of(provider_source, "badConst", 0),
            "newName": "BAD_CONST"
        }),
    );
    let response = server.response(&request_id);
    let error = response
        .error
        .expect("nested source content with a candidate must fail closed");
    assert!(
        error.message.to_ascii_lowercase().contains("include"),
        "unexpected nested candidate error: {error:?}"
    );
    server.shutdown();
}

#[test]
fn public_rename_rejects_a_missing_nested_include_without_parent_candidate_tokens() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path().join("fixture");
    let provider = root.join("Provider.pas");
    let consumer = root.join("Consumer.pas");
    let parent = root.join("Parent.inc");
    let provider_source =
        "unit Provider;\ninterface\nconst\n  badConst = 1;\nimplementation\nend.\n";
    let consumer_source = "unit Consumer;\ninterface\nimplementation\n{$I Parent.inc}\nend.\n";
    let parent_source = "procedure Irrelevant;\nbegin\nend;\n{$I MissingNested.inc}\n";
    write_file(&provider, provider_source);
    write_file(&consumer, consumer_source);
    write_file(&parent, parent_source);

    let mut server = TestServer::launch();
    server.initialize(&root, Value::Null);
    let request_id = RequestId::from("nested-missing-source-include-rename".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/rename",
        json!({
            "textDocument": {"uri": uri(&provider)},
            "position": position_of(provider_source, "badConst", 0),
            "newName": "BAD_CONST"
        }),
    );
    let response = server.response(&request_id);
    let error = response
        .error
        .expect("a missing nested include must fail closed");
    assert!(
        error.message.to_ascii_lowercase().contains("include"),
        "unexpected nested missing include error: {error:?}"
    );
    server.shutdown();
}

#[test]
fn public_rename_applies_retained_limits_only_to_relevant_sources() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path().join("fixture");
    let provider = root.join("Provider.pas");
    let consumer = root.join("Consumer.pas");
    let provider_source =
        "unit Provider;\ninterface\nconst\n  badConst = 1;\nimplementation\nend.\n";
    let consumer_source = "unit Consumer;\ninterface\nuses Provider;\nimplementation\nprocedure Use;\nbegin\n  Log(badConst);\nend;\nend.\n";
    write_file(&provider, provider_source);
    write_file(&consumer, consumer_source);
    for index in 0..80 {
        write_file(
            &root.join(format!("Noise{index:02}.pas")),
            &format!("unit Noise{index:02};\ninterface\nimplementation\nend.\n"),
        );
    }

    let mut server = TestServer::launch();
    server.initialize(&root, json!({"maxFiles": 2}));
    let request_id = RequestId::from("public-rename-retained-cap".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/rename",
        json!({
            "textDocument": {"uri": uri(&provider)},
            "position": position_of(provider_source, "badConst", 0),
            "newName": "BAD_CONST"
        }),
    );
    let response = server.response(&request_id);
    assert!(
        response.error.is_none(),
        "unrelated sources must not consume the retained source budget: {response:?}"
    );
    assert_eq!(
        workspace_edit_uris(&response.result.expect("public rename result")),
        HashSet::from([uri(&provider).to_string(), uri(&consumer).to_string()])
    );
    server.shutdown();
}

#[cfg(not(windows))]
#[test]
fn public_rename_never_deduplicates_case_distinct_linux_source_paths() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path().join("fixture");
    let provider = root.join("Provider.pas");
    let filename_upper = root.join("Consumer.pas");
    let filename_lower = root.join("consumer.pas");
    let directory_upper = root.join("Lib").join("Consumer.pas");
    let directory_lower = root.join("lib").join("Consumer.pas");
    let provider_source = "unit Provider;\ninterface\nconst badConst = 1;\nimplementation\nend.\n";
    let consumer_source = "unit Consumer;\ninterface\nuses Provider;\nimplementation\nprocedure Use;\nbegin\n  WriteLn(badConst);\nend;\nend.\n";
    write_file(&provider, provider_source);
    for path in [
        &filename_upper,
        &filename_lower,
        &directory_upper,
        &directory_lower,
    ] {
        write_file(path, consumer_source);
    }

    let mut server = TestServer::launch();
    server.initialize(&root, Value::Null);
    let request_id = RequestId::from("case-distinct-path-rename".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/rename",
        json!({
            "textDocument": {"uri": uri(&provider)},
            "position": position_of(provider_source, "badConst", 0),
            "newName": "BAD_CONST"
        }),
    );
    let response = server.response(&request_id);
    if let Some(error) = response.error {
        let message = error.message.to_ascii_lowercase();
        assert!(
            message.contains("case")
                || message.contains("collision")
                || message.contains("ambiguous"),
            "case-collision refusal must explain the ambiguity: {error:?}"
        );
    } else {
        assert_eq!(
            workspace_edit_uris(&response.result.expect("rename result")),
            HashSet::from([
                uri(&provider).to_string(),
                uri(&filename_upper).to_string(),
                uri(&filename_lower).to_string(),
                uri(&directory_upper).to_string(),
                uri(&directory_lower).to_string(),
            ])
        );
    }
    server.shutdown();
}

#[cfg(target_os = "linux")]
#[test]
fn rename_revalidates_every_scanned_source_before_returning_edits() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path().join("fixture");
    let provider = root.join("Provider.pas");
    let consumer = root.join("Consumer.pas");
    let provider_source =
        "unit Provider;\ninterface\nconst\n  badConst = 1;\nimplementation\nend.\n";
    let original_consumer_source = "unit Consumer;\ninterface\nuses Provider;\nimplementation\nprocedure Use;\nbegin\n  Log(0);\nend;\nend.\n";
    let changed_consumer_source = original_consumer_source.replace("Log(0)", "Log(badConst)");
    write_file(&provider, provider_source);
    write_file(&consumer, original_consumer_source);
    for index in 0..400 {
        write_file(
            &root.join(format!("Noise{index:04}.pas")),
            &format!("unit Noise{index:04};\ninterface\nimplementation\nend.\n"),
        );
    }

    let watch_path = CString::new(consumer.to_string_lossy().as_bytes()).expect("watch path");
    let fd = unsafe { inotify_init1(0) };
    assert!(fd >= 0, "inotify_init1 failed");
    let watch = unsafe { inotify_add_watch(fd, watch_path.as_ptr(), IN_CLOSE_NOWRITE) };
    assert!(watch >= 0, "inotify_add_watch failed");
    let consumer_for_watcher = consumer.clone();
    let watcher = thread::spawn(move || {
        let mut file = unsafe { std::fs::File::from_raw_fd(fd) };
        let mut events = [0_u8; 4096];
        let bytes = std::io::Read::read(&mut file, &mut events).expect("read inotify event");
        assert!(bytes > 0, "consumer read must produce a close event");
        write_file(&consumer_for_watcher, &changed_consumer_source);
    });

    let mut server = TestServer::launch();
    server.initialize(&root, Value::Null);
    let request_id = RequestId::from("read-set-race-rename".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/rename",
        json!({
            "textDocument": {"uri": uri(&provider)},
            "position": position_of(provider_source, "badConst", 0),
            "newName": "BAD_CONST"
        }),
    );
    let response = server.response(&request_id);
    watcher.join().expect("watcher must finish");
    let error = response
        .error
        .expect("a scanned source changed before the result and must invalidate the rename");
    assert_eq!(error.code, -32803);
    server.shutdown();
}

#[cfg(target_os = "linux")]
#[test]
fn rename_rejects_a_target_source_change_after_scope_classification() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path().join("fixture");
    let provider = root.join("Provider.pas");
    let consumer = root.join("Consumer.pas");
    let local_source = format!(
        "unit Provider;\ninterface\nimplementation procedure Use;\nvar   badConst: Integer;\nbegin badConst := 1; end;\nend.\n{{{}}}\n",
        "x".repeat(400_000)
    );
    let public_source = "unit Provider;\ninterface\n\nconst badConst = 1;\nimplementation\nend.\n";
    let consumer_source = "unit Consumer;\ninterface\nuses Provider;\nimplementation\nprocedure Use;\nbegin\n  Log(badConst);\nend;\nend.\n";
    write_file(&provider, &local_source);
    write_file(&consumer, consumer_source);

    let watch_path = CString::new(provider.to_string_lossy().as_bytes()).expect("watch path");
    let fd = unsafe { inotify_init1(0) };
    assert!(fd >= 0, "inotify_init1 failed");
    let watch = unsafe { inotify_add_watch(fd, watch_path.as_ptr(), IN_CLOSE_NOWRITE) };
    assert!(watch >= 0, "inotify_add_watch failed");
    let provider_for_watcher = provider.clone();
    let public_for_watcher = public_source.to_owned();
    let watcher = thread::spawn(move || {
        wait_for_close_events(fd, 2);
        write_file(&provider_for_watcher, &public_for_watcher);
    });

    let mut server = TestServer::launch();
    server.initialize(&root, Value::Null);
    let request_id = RequestId::from("scope-classification-race".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/rename",
        json!({
            "textDocument": {"uri": uri(&provider)},
            "position": position_of(&local_source, "badConst", 0),
            "newName": "RENAMED_CONST"
        }),
    );
    watcher.join().expect("scope race watcher must finish");
    let response = server.response(&request_id);
    let error = response
        .error
        .expect("a source change after classification must invalidate the rename");
    assert_eq!(error.code, -32803);
    assert!(
        error.message.to_ascii_lowercase().contains("changed")
            || error.message.to_ascii_lowercase().contains("stale"),
        "unexpected scope race error: {}",
        error.message
    );
    server.shutdown();
}

#[cfg(target_os = "linux")]
#[test]
fn rename_revalidates_filtered_source_content_with_equal_metadata() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path().join("fixture");
    let provider = root.join("Provider.pas");
    let consumer = root.join("Consumer.pas");
    let provider_source =
        "unit Provider;\ninterface\nconst\n  badConst = 1;\nimplementation\nend.\n";
    let original_consumer_source = "unit Consumer;\ninterface\nuses Provider;\nimplementation\nprocedure Use;\nbegin\n  Log(00000000);\nend;\nend.\n";
    let changed_consumer_source =
        original_consumer_source.replace("Log(00000000)", "Log(badConst)");
    write_file(&provider, provider_source);
    write_file(&consumer, original_consumer_source);
    for index in 0..600 {
        write_file(
            &root.join(format!("Noise{index:04}.pas")),
            &format!("unit Noise{index:04};\ninterface\nimplementation\nend.\n"),
        );
    }
    let original_metadata = fs::metadata(&consumer).expect("consumer metadata");

    let watch_path = CString::new(consumer.to_string_lossy().as_bytes()).expect("watch path");
    let fd = unsafe { inotify_init1(0) };
    assert!(fd >= 0, "inotify_init1 failed");
    let watch = unsafe { inotify_add_watch(fd, watch_path.as_ptr(), IN_CLOSE_NOWRITE) };
    assert!(watch >= 0, "inotify_add_watch failed");
    let consumer_for_watcher = consumer.clone();
    let changed_for_watcher = changed_consumer_source.clone();
    let watcher = thread::spawn(move || {
        wait_for_close_events(fd, 1);
        write_file(&consumer_for_watcher, &changed_for_watcher);
        restore_mtime(&consumer_for_watcher, &original_metadata);
    });

    let mut server = TestServer::launch();
    server.initialize(&root, Value::Null);
    let request_id = RequestId::from("equal-metadata-content-race".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/rename",
        json!({
            "textDocument": {"uri": uri(&provider)},
            "position": position_of(provider_source, "badConst", 0),
            "newName": "BAD_CONST"
        }),
    );
    watcher.join().expect("equal-metadata watcher must finish");
    let response = server.response(&request_id);
    let error = response
        .error
        .expect("filtered source content changes must invalidate the rename");
    assert_eq!(error.code, -32803);
    assert!(
        error.message.to_ascii_lowercase().contains("changed")
            || error.message.to_ascii_lowercase().contains("metadata"),
        "unexpected equal-metadata error: {}",
        error.message
    );
    server.shutdown();
}

#[cfg(target_os = "linux")]
#[test]
fn rename_revalidates_project_metadata_from_the_pre_read_baseline() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path().join("fixture");
    let provider = root.join("Provider.pas");
    let consumer = root.join("Consumer.pas");
    let watched_source = root.join("Noise0000.pas");
    let provider_source =
        "unit Provider;\ninterface\nconst\n  badConst = 1;\nimplementation\nend.\n";
    let consumer_source = "unit Consumer;\ninterface\nuses Provider;\nimplementation\nprocedure Use;\nbegin\n  Log(badConst);\nend;\nend.\n";
    write_file(&provider, provider_source);
    write_file(&consumer, consumer_source);
    for index in 0..400 {
        write_file(
            &root.join(format!("Noise{index:04}.pas")),
            &format!("unit Noise{index:04};\ninterface\nimplementation\nend.\n"),
        );
    }
    let project = root.join("App.dproj");
    write_file(
        &project,
        "<Project><PropertyGroup><DCC_UnitAlias>Provider=Provider</DCC_UnitAlias></PropertyGroup></Project>",
    );

    let watch_path = CString::new(watched_source.to_string_lossy().as_bytes()).expect("watch path");
    let fd = unsafe { inotify_init1(0) };
    assert!(fd >= 0, "inotify_init1 failed");
    let watch = unsafe { inotify_add_watch(fd, watch_path.as_ptr(), IN_CLOSE_NOWRITE) };
    assert!(watch >= 0, "inotify_add_watch failed");
    let project_for_watcher = project.clone();
    let watcher = thread::spawn(move || {
        let mut file = unsafe { std::fs::File::from_raw_fd(fd) };
        let mut events = [0_u8; 4096];
        let bytes = std::io::Read::read(&mut file, &mut events).expect("read inotify event");
        assert!(bytes > 0, "noise read must produce a close event");
        write_file(
            &project_for_watcher,
            "<Project><PropertyGroup><DCC_UnitAlias>Provider=Other</DCC_UnitAlias></PropertyGroup></Project>",
        );
    });

    let mut server = TestServer::launch();
    server.initialize(&root, json!({"projectFile": "App.dproj"}));
    let request_id = RequestId::from("metadata-baseline-race".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/rename",
        json!({
            "textDocument": {"uri": uri(&provider)},
            "position": position_of(provider_source, "badConst", 0),
            "newName": "BAD_CONST"
        }),
    );
    let response = server.response(&request_id);
    watcher.join().expect("metadata watcher must finish");
    let error = response
        .error
        .expect("metadata changed after the baseline and must invalidate the rename");
    assert_eq!(error.code, -32803);
    assert!(
        error.message.to_ascii_lowercase().contains("metadata")
            || error.message.to_ascii_lowercase().contains("changed")
            || error.message.to_ascii_lowercase().contains("stale"),
        "unexpected metadata race error: {}",
        error.message
    );
    server.shutdown();
}

#[test]
fn rename_normalizes_percent_encoded_file_uris_before_snapshot_lookup() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path().join("root@encoded");
    let main = root.join("Main.pas");
    let source = "unit Main;\ninterface\nconst\n  badConst = 1;\nimplementation\nend.\n";
    write_file(&main, source);

    let mut server = TestServer::launch();
    server.initialize(&root, Value::Null);
    let request_id = RequestId::from("percent-encoded-rename".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/rename",
        json!({
            "textDocument": {"uri": uri(&main)},
            "position": position_of(source, "badConst", 0),
            "newName": "BAD_CONST"
        }),
    );
    let response = server.response(&request_id);
    assert!(
        response.error.is_none(),
        "encoded file URI must resolve: {response:?}"
    );
    assert_eq!(
        workspace_edit_uris(&response.result.expect("encoded rename result")),
        HashSet::from([uri(&main).to_string()])
    );
    server.shutdown();
}

#[test]
fn code_action_resolve_normalizes_percent_encoded_file_uris() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path().join("root@encoded");
    let main = root.join("Main.pas");
    let source = "unit Main;\ninterface\nconst\n  badConst = 1;\nimplementation\nend.\n";
    write_file(
        &root.join(".lint4d.toml"),
        "[rules.naming]\nconstant_style = \"UPPER_CASE\"\n",
    );
    write_file(&main, source);
    let start = position_of(source, "badConst", 0);
    let end = Position::new(start.line, start.character + 8);

    let mut server = TestServer::launch();
    server.initialize_with_resolve_properties(&root, Value::Null, json!(["edit"]));
    let request_id = RequestId::from("encoded-code-action".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/codeAction",
        json!({
            "textDocument": {"uri": uri(&main)},
            "range": {"start": start, "end": end},
            "context": {
                "diagnostics": [{
                    "range": {"start": start, "end": end},
                    "severity": 4,
                    "code": "constant-naming",
                    "source": "lint4d",
                    "message": "naming violation"
                }],
                "only": ["quickfix"]
            }
        }),
    );
    let response = server.response(&request_id);
    assert!(response.error.is_none(), "codeAction failed: {response:?}");
    let action = response.result.expect("encoded code action")[0].clone();
    assert!(action["edit"].is_null());

    let resolve_id = RequestId::from("encoded-code-action-resolve".to_string());
    server.send_request(resolve_id.clone(), "codeAction/resolve", action);
    let resolved = server.response(&resolve_id);
    assert!(
        resolved.error.is_none(),
        "codeAction resolve failed: {resolved:?}"
    );
    assert!(resolved.result.expect("resolved action")["edit"].is_object());
    server.shutdown();
}

#[test]
fn rename_rejects_ambiguous_project_selection_instead_of_using_standalone_bindings() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path().join("fixture");
    let provider = root.join("Provider.pas");
    let other = root.join("Other.pas");
    let consumer = root.join("Consumer.pas");
    let provider_source =
        "unit Provider;\ninterface\nconst\n  badConst = 1;\nimplementation\nend.\n";
    let other_source = "unit Other;\ninterface\nconst\n  badConst = 2;\nimplementation\nend.\n";
    let consumer_source = "unit Consumer;\ninterface\nuses Provider;\nimplementation\nprocedure Use;\nbegin\n  Log(badConst);\nend;\nend.\n";
    write_file(&provider, provider_source);
    write_file(&other, other_source);
    write_file(&consumer, consumer_source);
    for name in ["A.dproj", "B.dproj"] {
        write_file(
            &root.join(name),
            "<Project><PropertyGroup><DCC_UnitAlias>Provider=Other</DCC_UnitAlias></PropertyGroup></Project>",
        );
    }

    let mut server = TestServer::launch();
    server.initialize(&root, Value::Null);
    let request_id = RequestId::from("ambiguous-project-rename".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/rename",
        json!({
            "textDocument": {"uri": uri(&provider)},
            "position": position_of(provider_source, "badConst", 0),
            "newName": "BAD_CONST"
        }),
    );
    let response = server.response(&request_id);
    let error = response
        .error
        .expect("ambiguous project selection must fail closed");
    assert!(
        error.message.to_ascii_lowercase().contains("ambiguous")
            || error.message.to_ascii_lowercase().contains("incomplete"),
        "unexpected error: {}",
        error.message
    );
    server.shutdown();

    let mut server = TestServer::launch();
    server.initialize(&root, json!({"projectFile": "A.dproj"}));
    let request_id = RequestId::from("explicit-project-rename".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/rename",
        json!({
            "textDocument": {"uri": uri(&provider)},
            "position": position_of(provider_source, "badConst", 0),
            "newName": "BAD_CONST"
        }),
    );
    let response = server.response(&request_id);
    assert!(
        response.error.is_none(),
        "explicit project must resolve: {response:?}"
    );
    assert_eq!(
        workspace_edit_uris(&response.result.expect("explicit project rename result")),
        HashSet::from([uri(&provider).to_string()])
    );
    server.shutdown();
}

#[test]
fn rename_rejects_an_unsaved_reverse_consumer_that_was_rejected_by_limits() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path().join("fixture");
    let provider = root.join("Provider.pas");
    let consumer = root.join("Consumer.pas");
    let provider_source =
        "unit Provider;\ninterface\nconst\n  badConst = 1;\nimplementation\nend.\n";
    let consumer_source = "unit Consumer;\ninterface\nuses Provider;\nimplementation\nprocedure Use;\nbegin\n  Log(badConst);\nend;\nend.\n";
    write_file(&provider, provider_source);

    let mut server = TestServer::launch();
    server.initialize(&root, json!({"maxFileBytes": 128}));
    let rejected_source = format!("{consumer_source}{}", "x".repeat(128));
    server.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri(&consumer),
                "languageId": "pascal",
                "version": 1,
                "text": rejected_source
            }
        }),
    );
    let rejection = server.notification("textDocument/publishDiagnostics");
    assert!(
        rejection["diagnostics"][0]["message"]
            .as_str()
            .expect("rejection diagnostic")
            .contains("per-file limit")
    );

    let request_id = RequestId::from("rejected-unsaved-consumer-rename".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/rename",
        json!({
            "textDocument": {"uri": uri(&provider)},
            "position": position_of(provider_source, "badConst", 0),
            "newName": "BAD_CONST"
        }),
    );
    let response = server.response(&request_id);
    let error = response
        .error
        .expect("rejected unsaved consumer must prevent a partial rename");
    assert!(
        error.message.to_ascii_lowercase().contains("rejected")
            || error.message.to_ascii_lowercase().contains("incomplete"),
        "unexpected error: {}",
        error.message
    );
    server.shutdown();
}

#[cfg(unix)]
#[test]
fn rename_prunes_excluded_subtrees_before_traversal_and_source_budgets() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path().join("fixture");
    let main = root.join("Main.pas");
    let ignored = root.join("ignored");
    let source = "unit Main;\ninterface\nconst\n  badConst = 1;\nimplementation\nend.\n";
    write_file(&main, source);
    write_file(
        &ignored.join("Nested.pas"),
        "unit Nested;\ninterface\nimplementation\nend.\n",
    );
    let mut permissions = fs::metadata(&ignored)
        .expect("ignored directory metadata")
        .permissions();
    permissions.set_mode(0o0);
    fs::set_permissions(&ignored, permissions).expect("make excluded directory unreadable");

    let mut server = TestServer::launch();
    server.initialize(&root, json!({"exclude": ["ignored"], "maxFiles": 1}));
    let request_id = RequestId::from("excluded-subtree-rename".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/rename",
        json!({
            "textDocument": {"uri": uri(&main)},
            "position": position_of(source, "badConst", 0),
            "newName": "BAD_CONST"
        }),
    );
    let response = server.response(&request_id);
    let mut restore = fs::metadata(&ignored)
        .expect("ignored directory metadata")
        .permissions();
    restore.set_mode(0o755);
    fs::set_permissions(&ignored, restore).expect("restore excluded directory permissions");
    assert!(
        response.error.is_none(),
        "excluded unreadable subtree must not make the retained source incomplete: {response:?}"
    );
    server.shutdown();
}

#[test]
fn rename_scans_beyond_the_retained_file_budget() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path().join("fixture");
    let main = root.join("Main.pas");
    let source = "unit Main;\ninterface\nconst\n  badConst = 1;\nimplementation\nend.\n";
    write_file(&main, source);
    fs::create_dir_all(&root).expect("create workspace root");
    for index in 0..5_000 {
        fs::write(
            root.join(format!("unrelated-{index:04}.txt")),
            "not a Pascal source",
        )
        .expect("write unrelated entry");
    }

    let mut server = TestServer::launch();
    server.initialize(&root, json!({"maxFiles": 1}));
    let request_id = RequestId::from("bounded-traversal-rename".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/rename",
        json!({
            "textDocument": {"uri": uri(&main)},
            "position": position_of(source, "badConst", 0),
            "newName": "BAD_CONST"
        }),
    );
    let response = server.response(&request_id);
    assert!(
        response.error.is_none(),
        "unrelated traversal entries must not consume the retained file budget: {response:?}"
    );
    assert_eq!(
        workspace_edit_uris(&response.result.expect("rename result")),
        HashSet::from([uri(&main).to_string()])
    );
    server.shutdown();
}

#[test]
fn code_actions_eagerly_include_edits_unless_edit_resolution_is_advertised() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path().join("fixture");
    let main = root.join("Main.pas");
    let source = "unit Main;\ninterface\nconst\n  badConst = 1;\nimplementation\nend.\n";
    write_file(
        &root.join(".lint4d.toml"),
        "[rules.naming]\nconstant_style = \"UPPER_CASE\"\n",
    );
    write_file(&main, source);
    let start = position_of(source, "badConst", 0);
    let end = Position::new(start.line, start.character + 8);

    for (label, properties, expect_eager) in [
        ("empty", json!([]), true),
        ("other", json!(["command"]), true),
        ("edit", json!(["edit"]), false),
    ] {
        let mut server = TestServer::launch();
        server.initialize_with_resolve_properties(&root, Value::Null, properties);
        let request_id = RequestId::from(format!("resolve-properties-{label}"));
        server.send_request(
            request_id.clone(),
            "textDocument/codeAction",
            json!({
                "textDocument": {"uri": uri(&main)},
                "range": {"start": start, "end": end},
                "context": {
                    "diagnostics": [{
                        "range": {"start": start, "end": end},
                        "severity": 4,
                        "code": "constant-naming",
                        "source": "lint4d",
                        "message": "naming violation"
                    }],
                    "only": ["quickfix"]
                }
            }),
        );
        let response = server.response(&request_id);
        assert!(response.error.is_none(), "codeAction failed: {response:?}");
        let action = &response.result.expect("actions")[0];
        assert_eq!(
            action["edit"].is_object(),
            expect_eager,
            "resolve properties {label} must negotiate edit ownership"
        );
        server.shutdown();
    }
}

#[test]
fn rename_allows_harmless_compiler_directive_includes_and_unrelated_conditionals() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path().join("fixture");
    let main = root.join("Main.pas");
    let include = root.join("BuildDirectives.inc");
    let source = "unit Main;\ninterface\n{$I BuildDirectives.inc}\nconst\n  badConst = 1;\n{$IFDEF FEATURE}\nconst\n  unrelatedValue = 2;\n{$ENDIF}\nimplementation\nend.\n";
    write_file(&include, "{$DEFINE FEATURE}\n{$METHODINFO ON}\n");
    write_file(&main, source);

    let mut server = TestServer::launch();
    server.initialize(&root, Value::Null);
    let request_id = RequestId::from("harmless-directives-rename".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/rename",
        json!({
            "textDocument": {"uri": uri(&main)},
            "position": position_of(source, "badConst", 0),
            "newName": "BAD_CONST"
        }),
    );
    let response = server.response(&request_id);
    assert!(
        response.error.is_none(),
        "harmless directives must not blanket-reject rename: {response:?}"
    );
    assert_eq!(
        workspace_edit_uris(&response.result.expect("rename result")),
        HashSet::from([uri(&main).to_string()])
    );
    server.shutdown();
}

#[test]
fn rename_resolves_includes_from_source_then_ordered_project_paths() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path().join("fixture");
    let source_dir = root.join("src");
    let include_one = root.join("include-one");
    let include_two = root.join("include-two");
    let main = source_dir.join("Main.pas");
    let source = "unit Main;\ninterface\n{$I Shared.inc}\n{$I Ordered.inc}\nconst\n  badConst = 1;\nimplementation\nend.\n";

    write_file(
        &source_dir.join("Shared.inc"),
        "// source-directory include\n{$DEFINE LOCAL}\n",
    );
    write_file(
        &include_one.join("Shared.inc"),
        "procedure MustNotBeSelected; begin Log(badConst); end;\n",
    );
    write_file(
        &include_one.join("Ordered.inc"),
        "{$IFDEF FIRST}\n{$DEFINE FIRST_VALUE}\n{$ENDIF}\n",
    );
    write_file(
        &include_two.join("Ordered.inc"),
        "procedure MustNotBeSelected; begin Log(badConst); end;\n",
    );
    write_file(&main, source);
    write_file(&root.join("App.dpr"), "program App; begin end.\n");
    write_file(
        &root.join("App.dproj"),
        "<Project><PropertyGroup><MainSource>App.dpr</MainSource><DCC_IncludePath>include-one;include-two</DCC_IncludePath><DCCReference Include=\"src\\Main.pas\"/></PropertyGroup></Project>",
    );

    let mut server = TestServer::launch();
    server.initialize(
        &root,
        json!({"projectFile": "App.dproj", "sourcePaths": ["src"]}),
    );
    let request_id = RequestId::from("ordered-include-rename".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/rename",
        json!({
            "textDocument": {"uri": uri(&main)},
            "position": position_of(source, "badConst", 0),
            "newName": "BAD_CONST"
        }),
    );
    let response = server.response(&request_id);
    assert!(
        response.error.is_none(),
        "source and ordered project includes must resolve safely: {response:?}"
    );
    assert_eq!(
        workspace_edit_uris(&response.result.expect("rename result")),
        HashSet::from([uri(&main).to_string()])
    );
    server.shutdown();
}

#[test]
fn rename_falls_back_to_delphi_unit_then_client_source_paths_for_includes() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path().join("fixture");
    let unit_include = root.join("unit-includes");
    let client_include = root.join("client-includes");
    let main = root.join("src/Main.pas");
    let source =
        "unit Main;\ninterface\n{$I Fallback.inc}\nconst\n  badConst = 1;\nimplementation\nend.\n";

    write_file(
        &unit_include.join("Fallback.inc"),
        "{$IFDEF UNIT_PATH}\n{$DEFINE UNIT_VALUE}\n{$ENDIF}\n",
    );
    write_file(
        &client_include.join("Fallback.inc"),
        "procedure MustNotBeSelected; begin Log(badConst); end;\n",
    );
    write_file(&main, source);
    write_file(&root.join("App.dpr"), "program App; begin end.\n");
    write_file(
        &root.join("App.dproj"),
        "<Project><PropertyGroup><MainSource>App.dpr</MainSource><DCC_UnitSearchPath>unit-includes</DCC_UnitSearchPath><DCCReference Include=\"src\\Main.pas\"/></PropertyGroup></Project>",
    );

    let mut server = TestServer::launch();
    server.initialize(
        &root,
        json!({"projectFile": "App.dproj", "sourcePaths": ["client-includes"]}),
    );
    let request_id = RequestId::from("fallback-include-rename".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/rename",
        json!({
            "textDocument": {"uri": uri(&main)},
            "position": position_of(source, "badConst", 0),
            "newName": "BAD_CONST"
        }),
    );
    let response = server.response(&request_id);
    assert!(
        response.error.is_none(),
        "DCC_UnitSearchPath must precede client source paths for includes: {response:?}"
    );
    server.shutdown();
}

#[test]
fn rename_accepts_realistic_conditional_directive_include_and_edits_unopened_consumer() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path().join("fixture");
    let common = root.join("Common");
    let provider = root.join("src/Provider.pas");
    let consumer = root.join("src/Consumer.pas");
    let provider_source =
        "unit Provider;\ninterface\nconst\n  badConst = 1;\nimplementation\nend.\n";
    let consumer_source = "unit Consumer;\ninterface\nuses Provider;\n{$I MDCompilers.inc}\nimplementation\nprocedure Use;\nbegin\n  Log(badConst);\nend;\nend.\n";
    let compiler_include = "// compiler selection only\n{$IFDEF badConst}\n{$DEFINE TARGET_NAME_IS_A_COMPILER_SYMBOL}\n{$ENDIF}\n{$IFDEF LEGACY_COMPILER}\n{$DEFINE LEGACY}\n{$ELSEIF Defined(NEW_COMPILER)}\n{$DEFINE MODERN}\n{$ENDIF}\n{$IF CompilerVersion >= 24}\n{$DEFINE DELPHI_XE3_UP}\n{$ENDIF}\n{$IFDEF CPPB_3_UP}\n{$ObjExportAll On}\n{$ENDIF}\n";

    write_file(&provider, provider_source);
    write_file(&consumer, consumer_source);
    write_file(&common.join("MDCompilers.inc"), compiler_include);
    write_file(&root.join("App.dpr"), "program App; begin end.\n");
    write_file(
        &root.join("App.dproj"),
        "<Project><PropertyGroup><MainSource>App.dpr</MainSource><DCCReference Include=\"src\\Provider.pas\"/><DCCReference Include=\"src\\Consumer.pas\"/></PropertyGroup></Project>",
    );

    let mut server = TestServer::launch();
    server.initialize(
        &root,
        json!({"projectFile": "App.dproj", "sourcePaths": ["Common"]}),
    );
    let request_id = RequestId::from("conditional-include-unopened-consumer".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/rename",
        json!({
            "textDocument": {"uri": uri(&provider)},
            "position": position_of(provider_source, "badConst", 0),
            "newName": "BAD_CONST"
        }),
    );
    let response = server.response(&request_id);
    assert!(
        response.error.is_none(),
        "directive-only conditional include must not block rename: {response:?}"
    );
    assert_eq!(
        workspace_edit_uris(&response.result.expect("rename result")),
        HashSet::from([uri(&provider).to_string(), uri(&consumer).to_string()])
    );
    server.shutdown();
}

#[test]
fn rename_rejects_pascal_symbol_in_declared_include_expression() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path().join("fixture");
    let main = root.join("Main.pas");
    let source =
        "unit Main;\ninterface\nconst\n  badConst = 1;\nimplementation\n{$I Shared.inc}\nend.\n";
    write_file(
        &root.join("Shared.inc"),
        "{$IF Declared(badConst)}\n{$DEFINE EXISTS}\n{$ENDIF}\n",
    );
    write_file(&main, source);

    let mut server = TestServer::launch();
    server.initialize(&root, Value::Null);
    let request_id = RequestId::from("declared-include-expression-rename".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/rename",
        json!({
            "textDocument": {"uri": uri(&main)},
            "position": position_of(source, "badConst", 0),
            "newName": "GOOD_CONST"
        }),
    );
    let response = server.response(&request_id);
    let error = response
        .error
        .expect("Pascal-dependent Declared expression must fail closed");
    assert_eq!(error.code, -32803);
    assert!(error.message.to_ascii_lowercase().contains("include"));
    server.shutdown();
}

#[test]
fn rename_rejects_pascal_symbol_in_elseif_include_expression() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path().join("fixture");
    let main = root.join("Main.pas");
    let source =
        "unit Main;\ninterface\nconst\n  badConst = 1;\nimplementation\n{$I Shared.inc}\nend.\n";
    write_file(
        &root.join("Shared.inc"),
        "{$IF CompilerVersion >= 24}\n{$DEFINE SAFE}\n{$ELSEIF badConst = 1}\n{$DEFINE TARGET}\n{$ENDIF}\n",
    );
    write_file(&main, source);

    let mut server = TestServer::launch();
    server.initialize(&root, Value::Null);
    let request_id = RequestId::from("elseif-include-expression-rename".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/rename",
        json!({
            "textDocument": {"uri": uri(&main)},
            "position": position_of(source, "badConst", 0),
            "newName": "GOOD_CONST"
        }),
    );
    let response = server.response(&request_id);
    let error = response
        .error
        .expect("Pascal-dependent ELSEIF expression must fail closed");
    assert_eq!(error.code, -32803);
    assert!(error.message.to_ascii_lowercase().contains("include"));
    server.shutdown();
}

#[test]
fn rename_rejects_excessive_nested_include_depth_without_crashing() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path().join("fixture");
    let main = root.join("Main.pas");
    let source =
        "unit Main;\ninterface\nconst\n  badConst = 1;\nimplementation\n{$I chain0.inc}\nend.\n";
    write_file(&main, source);
    for index in 0..500 {
        let body = if index == 499 {
            "{$DEFINE SAFE}\n".to_string()
        } else {
            format!("{{$I chain{}.inc}}\n", index + 1)
        };
        write_file(&root.join(format!("chain{index}.inc")), &body);
    }

    let mut server = TestServer::launch();
    server.initialize(&root, Value::Null);
    let request_id = RequestId::from("nested-include-depth-rename".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/rename",
        json!({
            "textDocument": {"uri": uri(&main)},
            "position": position_of(source, "badConst", 0),
            "newName": "GOOD_CONST"
        }),
    );
    let response = server.response(&request_id);
    let error = response
        .error
        .expect("excessive include depth must fail closed");
    assert_eq!(error.code, -32803);
    assert!(error.message.to_ascii_lowercase().contains("depth"));
    server.shutdown();
}

#[test]
fn public_rename_accounts_for_bounded_include_owner_summaries() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path().join("fixture");
    let provider = root.join("Provider.pas");
    let consumer = root.join("Consumer.pas");
    let provider_source =
        "unit Provider;\ninterface\nconst\n  badConst = 1;\nimplementation\nend.\n";
    let consumer_source = "unit Consumer;\ninterface\nimplementation\n{$I Shared.inc}\nend.\n";
    write_file(&provider, provider_source);
    write_file(&consumer, consumer_source);
    write_file(&root.join("Shared.inc"), "{$DEFINE SAFE}\n");

    let mut server = TestServer::launch();
    server.initialize(&root, json!({"maxFiles": 1}));
    let request_id = RequestId::from("include-owner-retained-limit".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/rename",
        json!({
            "textDocument": {"uri": uri(&provider)},
            "position": position_of(provider_source, "badConst", 0),
            "newName": "GOOD_CONST"
        }),
    );
    let response = server.response(&request_id);
    let error = response
        .error
        .expect("include owner retention must fail before omission");
    assert_eq!(error.code, -32803);
    assert!(
        error
            .message
            .to_ascii_lowercase()
            .contains("include owners")
    );
    server.shutdown();
}

#[cfg(target_os = "linux")]
#[test]
fn rename_revalidates_resolved_include_content_with_equal_metadata() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path().join("fixture");
    let main = root.join("Main.pas");
    let include = root.join("Shared.inc");
    let source = "unit Main;\ninterface\nimplementation\n{$I Shared.inc}\nprocedure Run;\nvar\n  badConst: Integer;\nbegin\n  badConst := 1;\nend;\nend.\n";
    write_file(&include, "{$DEFINE FEATURE}\n");
    write_file(&main, source);
    let original_metadata = fs::metadata(&include).expect("include metadata");

    let watch_path = CString::new(include.to_string_lossy().as_bytes()).expect("watch path");
    let fd = unsafe { inotify_init1(0) };
    assert!(fd >= 0, "inotify_init1 failed");
    let watch = unsafe { inotify_add_watch(fd, watch_path.as_ptr(), IN_CLOSE_NOWRITE) };
    assert!(watch >= 0, "inotify_add_watch failed");
    let include_for_watcher = include.clone();
    let watcher = thread::spawn(move || {
        wait_for_close_events(fd, 1);
        write_file(&include_for_watcher, "{$DEFINE CHANGED}\n");
        restore_mtime(&include_for_watcher, &original_metadata);
        assert_eq!(
            fs::metadata(&include_for_watcher)
                .expect("changed include metadata")
                .modified()
                .expect("changed include mtime"),
            original_metadata
                .modified()
                .expect("original include mtime")
        );
    });

    let mut server = TestServer::launch();
    server.initialize(&root, Value::Null);
    let request_id = RequestId::from("include-content-race".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/rename",
        json!({
            "textDocument": {"uri": uri(&main)},
            "position": position_of(source, "badConst", 0),
            "newName": "BAD_CONST"
        }),
    );
    let response = server.response(&request_id);
    watcher.join().expect("include watcher must finish");
    let error = response
        .error
        .expect("changed include content must invalidate the rename");
    assert_eq!(error.code, -32803);
    assert!(
        error.message.to_ascii_lowercase().contains("changed")
            || error.message.to_ascii_lowercase().contains("metadata"),
        "unexpected include race error: {}",
        error.message
    );
    server.shutdown();
}

#[test]
fn rename_rejects_branch_dependent_declarations_even_when_the_directive_parses() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path().join("fixture");
    let main = root.join("Main.pas");
    let source = "unit Main;\ninterface\n{$IFDEF FIRST}\nconst\n  badConst = 1;\n{$ELSEIF SECOND}\nconst\n  badConst = 2;\n{$ELSE}\nconst\n  badConst = 3;\n{$ENDIF}\nimplementation\nend.\n";
    write_file(&main, source);

    let mut server = TestServer::launch();
    server.initialize(&root, Value::Null);
    let request_id = RequestId::from("conditional-declaration-rename".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/rename",
        json!({
            "textDocument": {"uri": uri(&main)},
            "position": position_of(source, "badConst", 0),
            "newName": "BAD_CONST"
        }),
    );
    let response = server.response(&request_id);
    let error = response
        .error
        .expect("branch-dependent declarations must fail closed");
    assert!(
        error.message.to_ascii_lowercase().contains("conditional")
            || error.message.to_ascii_lowercase().contains("ambiguous"),
        "unexpected conditional declaration error: {}",
        error.message
    );
    server.shutdown();
}

#[test]
fn prepare_rename_rejects_an_unresolved_include_before_returning_a_range() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path().join("fixture");
    let main = root.join("Main.pas");
    let source = "unit Main;\ninterface\n{$I MissingGenerated.inc}\nconst\n  badConst = 1;\nimplementation\nend.\n";
    write_file(&main, source);

    let mut server = TestServer::launch();
    server.initialize(&root, Value::Null);
    let request_id = RequestId::from("unresolved-include-prepare".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/prepareRename",
        json!({
            "textDocument": {"uri": uri(&main)},
            "position": position_of(source, "badConst", 0)
        }),
    );
    let response = server.response(&request_id);
    let error = response
        .error
        .expect("prepareRename must reject an unresolved include");
    assert!(
        error.message.to_ascii_lowercase().contains("include"),
        "unexpected error: {}",
        error.message
    );
    server.shutdown();
}

#[test]
fn rename_refuses_sources_with_potentially_relevant_includes() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path().join("fixture");
    let main = root.join("Main.pas");
    let source =
        "unit Main;\ninterface\n{$I Generated.inc}\nconst\n  badConst = 1;\nimplementation\nend.\n";
    write_file(&main, source);

    let mut server = TestServer::launch();
    server.initialize(&root, Value::Null);
    let request_id = RequestId::from("include-rename".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/rename",
        json!({
            "textDocument": {"uri": uri(&main)},
            "position": position_of(source, "badConst", 0),
            "newName": "BAD_CONST"
        }),
    );
    let response = server.response(&request_id);
    let error = response.error.expect("include-sensitive rename must fail");
    assert!(error.message.to_ascii_lowercase().contains("include"));
    server.shutdown();
}

#[test]
fn public_rename_rejects_an_unresolved_include_in_a_consumer() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path().join("fixture");
    let provider = root.join("Provider.pas");
    let consumer = root.join("Consumer.pas");
    let provider_source =
        "unit Provider;\ninterface\nconst\n  badConst = 1;\nimplementation\nend.\n";
    let consumer_source = "unit Consumer;\ninterface\nuses Provider;\nimplementation\nprocedure Use;\nbegin\n  {$I MissingBody.inc}\nend;\nend.\n";
    write_file(&provider, provider_source);
    write_file(&consumer, consumer_source);

    let mut server = TestServer::launch();
    server.initialize(&root, Value::Null);
    let request_id = RequestId::from("unresolved-consumer-include-rename".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/rename",
        json!({
            "textDocument": {"uri": uri(&provider)},
            "position": position_of(provider_source, "badConst", 0),
            "newName": "BAD_CONST"
        }),
    );
    let response = server.response(&request_id);
    let error = response
        .error
        .expect("unresolved consumer include must fail closed");
    assert!(
        error.message.to_ascii_lowercase().contains("include"),
        "unexpected error: {}",
        error.message
    );
    server.shutdown();
}

#[test]
fn rename_refuses_sources_with_conditional_compilation() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path().join("fixture");
    let main = root.join("Main.pas");
    let source = "unit Main;\ninterface\nconst\n  badConst = 1;\nimplementation\nprocedure Use;\nbegin\n{$IFDEF FEATURE}\n  Log(badConst);\n{$ENDIF}\nend;\nend.\n";
    write_file(&main, source);

    let mut server = TestServer::launch();
    server.initialize(&root, Value::Null);
    let request_id = RequestId::from("conditional-rename".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/rename",
        json!({
            "textDocument": {"uri": uri(&main)},
            "position": position_of(source, "badConst", 0),
            "newName": "BAD_CONST"
        }),
    );
    let response = server.response(&request_id);
    let error = response
        .error
        .expect("conditional rename must fail conservatively");
    assert!(
        error.message.to_ascii_lowercase().contains("conditional"),
        "unexpected error: {}",
        error.message
    );
    server.shutdown();
}

#[cfg(unix)]
#[test]
fn rename_rejects_a_symlink_that_escapes_the_workspace_root() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path().join("fixture");
    let outside = temp.path().join("outside.pas");
    let link = root.join("Linked.pas");
    let source = "unit Outside;\ninterface\nconst\n  badConst = 1;\nimplementation\nend.\n";
    write_file(&outside, source);
    fs::create_dir_all(&root).expect("create workspace root");
    symlink(&outside, &link).expect("create source symlink");

    let mut server = TestServer::launch();
    server.initialize(&root, Value::Null);
    let request_id = RequestId::from("symlink-rename".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/rename",
        json!({
            "textDocument": {"uri": uri(&link)},
            "position": position_of(source, "badConst", 0),
            "newName": "BAD_CONST"
        }),
    );
    let response = server.response(&request_id);
    let error = response.error.expect("symlink escape must fail");
    assert!(
        error.message.to_ascii_lowercase().contains("workspace"),
        "unexpected error: {}",
        error.message
    );
    assert_eq!(
        fs::read_to_string(&outside).expect("outside source remains"),
        source
    );
    server.shutdown();
}

#[test]
fn rename_does_not_fall_back_to_disk_for_a_rejected_open_document() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path().join("fixture");
    let main = root.join("Main.pas");
    let disk_source = "unit Main;\ninterface\nconst\n  badConst = 1;\nimplementation\nend.\n";
    write_file(&main, disk_source);
    let oversized_overlay = format!("{disk_source}{}", "x".repeat(100));

    let mut server = TestServer::launch();
    server.initialize(&root, json!({"maxFileBytes": 128}));
    server.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri(&main),
                "languageId": "pascal",
                "version": 1,
                "text": oversized_overlay
            }
        }),
    );
    let rejection = server.notification("textDocument/publishDiagnostics");
    assert!(
        rejection["diagnostics"][0]["message"]
            .as_str()
            .expect("rejection diagnostic")
            .contains("per-file limit")
    );

    let request_id = RequestId::from("rejected-overlay-rename".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/rename",
        json!({
            "textDocument": {"uri": uri(&main)},
            "position": position_of(disk_source, "badConst", 0),
            "newName": "BAD_CONST"
        }),
    );
    let response = server.response(&request_id);
    assert!(
        response.error.is_some(),
        "rejected overlay must not use disk fallback"
    );
    server.shutdown();
}

#[test]
fn rename_rejects_a_target_in_an_external_source_path() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path().join("fixture");
    let external = temp.path().join("external");
    let provider = external.join("Provider.pas");
    let source = "unit Provider;\ninterface\nconst\n  badConst = 1;\nimplementation\nend.\n";
    fs::create_dir_all(&root).expect("create workspace root");
    write_file(&provider, source);

    let mut server = TestServer::launch();
    server.initialize(&root, json!({"sourcePaths": [external]}));
    let request_id = RequestId::from("external-source-rename".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/rename",
        json!({
            "textDocument": {"uri": uri(&provider)},
            "position": position_of(source, "badConst", 0),
            "newName": "BAD_CONST"
        }),
    );
    let response = server.response(&request_id);
    let error = response.error.expect("external source rename must fail");
    assert!(
        error.message.to_ascii_lowercase().contains("workspace"),
        "unexpected error: {}",
        error.message
    );
    assert_eq!(
        fs::read_to_string(&provider).expect("external source remains"),
        source
    );
    server.shutdown();
}

#[test]
fn rename_refuses_a_workspace_with_an_unresolved_import_context() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path().join("fixture");
    let provider = root.join("Provider.pas");
    let consumer = root.join("Consumer.pas");
    let provider_source =
        "unit Provider;\ninterface\nconst\n  badConst = 1;\nimplementation\nend.\n";
    let consumer_source = "unit Consumer;\ninterface\nuses MissingUnit, Provider;\nimplementation\nprocedure Use;\nbegin\n  Log(badConst);\nend;\nend.\n";
    write_file(&provider, provider_source);
    write_file(&consumer, consumer_source);

    let mut server = TestServer::launch();
    server.initialize(&root, Value::Null);
    let request_id = RequestId::from("unresolved-import-rename".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/rename",
        json!({
            "textDocument": {"uri": uri(&provider)},
            "position": position_of(provider_source, "badConst", 0),
            "newName": "BAD_CONST"
        }),
    );
    let response = server.response(&request_id);
    let error = response.error.expect("unresolved import must fail");
    assert!(error.message.to_ascii_lowercase().contains("incomplete"));
    server.shutdown();
}

#[test]
fn code_actions_respect_naming_suppressions() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path().join("fixture");
    let main = root.join("Main.pas");
    let source = "unit Main;\ninterface\nconst\n// lint4d:ignore constant-naming\n  badConst = 1;\nimplementation\nend.\n";
    write_file(&main, source);
    let start = position_of(source, "badConst", 0);
    let end = Position::new(start.line, start.character + 8);

    let mut server = TestServer::launch();
    server.initialize(&root, Value::Null);
    let request_id = RequestId::from("suppressed-code-action".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/codeAction",
        json!({
            "textDocument": {"uri": uri(&main)},
            "range": {"start": start, "end": end},
            "context": {"diagnostics": [], "only": ["quickfix"]}
        }),
    );
    let response = server.response(&request_id);
    assert!(response.error.is_none(), "codeAction failed: {response:?}");
    assert_eq!(response.result.expect("code actions"), json!([]));
    server.shutdown();
}

#[test]
fn code_actions_respect_disabled_naming_rules() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path().join("fixture");
    let main = root.join("Main.pas");
    let source = "unit Main;\ninterface\nconst\n  badConst = 1;\nimplementation\nend.\n";
    write_file(
        &root.join(".lint4d.toml"),
        "[rules]\n\"constant-naming\" = \"off\"\n",
    );
    write_file(&main, source);
    let start = position_of(source, "badConst", 0);
    let end = Position::new(start.line, start.character + 8);

    let mut server = TestServer::launch();
    server.initialize(&root, Value::Null);
    let request_id = RequestId::from("disabled-code-action".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/codeAction",
        json!({
            "textDocument": {"uri": uri(&main)},
            "range": {"start": start, "end": end},
            "context": {"diagnostics": [], "only": ["quickfix"]}
        }),
    );
    let response = server.response(&request_id);
    assert!(response.error.is_none(), "codeAction failed: {response:?}");
    assert_eq!(response.result.expect("code actions"), json!([]));
    server.shutdown();
}

#[test]
fn code_action_resolve_rejects_stale_configuration() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path().join("fixture");
    let main = root.join("Main.pas");
    let source = "unit Main;\ninterface\nconst\n  badConst = 1;\nimplementation\nend.\n";
    write_file(
        &root.join(".lint4d.toml"),
        "[rules.naming]\nconstant_style = \"UPPER_CASE\"\n",
    );
    write_file(&main, source);
    let start = position_of(source, "badConst", 0);
    let end = Position::new(start.line, start.character + 8);

    let mut server = TestServer::launch();
    server.initialize_with_action_support(&root, Value::Null);
    let action_id = RequestId::from("stale-config-actions".to_string());
    server.send_request(
        action_id.clone(),
        "textDocument/codeAction",
        json!({
            "textDocument": {"uri": uri(&main)},
            "range": {"start": start, "end": end},
            "context": {"diagnostics": [], "only": ["quickfix"]}
        }),
    );
    let action_response = server.response(&action_id);
    assert!(
        action_response.error.is_none(),
        "codeAction failed: {action_response:?}"
    );
    let action = action_response.result.expect("actions")[0].clone();

    write_file(
        &root.join(".lint4d.toml"),
        "[rules.naming]\nconstant_style = \"PascalCase\"\n",
    );
    let resolve_id = RequestId::from("stale-config-resolve".to_string());
    server.send_request(resolve_id.clone(), "codeAction/resolve", action);
    let response = server.response(&resolve_id);
    assert!(response.error.is_some(), "stale config must reject resolve");
    server.shutdown();
}

#[test]
fn code_actions_defer_incomplete_workspace_explanation_until_resolve() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path().join("fixture");
    let main = root.join("Main.pas");
    let other = root.join("Other.pas");
    let source = "unit Main;\ninterface\nconst\n  badConst = 1;\nimplementation\nend.\n";
    let other_source = "unit Other;\ninterface\nuses Main;\nimplementation\nprocedure Use;\nbegin\n  Log(badConst);\nend;\nend.\n";
    write_file(&main, source);
    write_file(&other, other_source);
    let start = position_of(source, "badConst", 0);
    let end = Position::new(start.line, start.character + 8);

    let mut server = TestServer::launch();
    server.initialize_with_action_support(&root, json!({"maxFiles": 1}));
    let request_id = RequestId::from("incomplete-code-actions".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/codeAction",
        json!({
            "textDocument": {"uri": uri(&main)},
            "range": {"start": start, "end": end},
            "context": {"diagnostics": [], "only": ["quickfix"]}
        }),
    );
    let response = server.response(&request_id);
    assert!(response.error.is_none(), "codeAction failed: {response:?}");
    let actions = response.result.expect("actions");
    assert_eq!(actions.as_array().expect("action array").len(), 1);
    assert!(actions[0]["edit"].is_null());
    assert!(actions[0]["disabled"].is_null());
    let resolve_id = RequestId::from("incomplete-code-action-resolve".to_string());
    server.send_request(resolve_id.clone(), "codeAction/resolve", actions[0].clone());
    let resolved = server.response(&resolve_id);
    assert!(resolved.error.is_none(), "resolve failed: {resolved:?}");
    assert!(
        resolved.result.expect("resolved action")["disabled"]["reason"]
            .as_str()
            .expect("disabled action reason")
            .contains("incomplete")
    );
    server.shutdown();
}

#[test]
fn rename_uses_legacy_changes_for_clients_without_document_changes() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path().join("fixture");
    let main = root.join("Main.pas");
    let source = "unit Main;\ninterface\nconst\n  badConst = 1;\nimplementation\nend.\n";
    write_file(&main, source);

    let mut server = TestServer::launch();
    server.initialize_without_document_changes(&root, Value::Null);
    let request_id = RequestId::from("legacy-rename".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/rename",
        json!({
            "textDocument": {"uri": uri(&main)},
            "position": position_of(source, "badConst", 0),
            "newName": "BAD_CONST"
        }),
    );
    let response = server.response(&request_id);
    assert!(response.error.is_none(), "rename failed: {response:?}");
    let result = response.result.expect("rename result");
    assert!(result["changes"].is_object());
    assert!(result["documentChanges"].is_null());
    server.shutdown();
}

#[test]
fn rename_cancellation_returns_the_standard_request_canceled_error() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path().join("fixture");
    let main = root.join("Main.pas");
    let source = "unit Main;\ninterface\nconst\n  badConst = 1;\nimplementation\nend.\n";
    write_file(&main, source);
    for index in 0..2_000 {
        write_file(
            &root.join(format!("Noise{index:04}.pas")),
            &format!("unit Noise{index:04};\ninterface\nimplementation\nend.\n"),
        );
    }

    let mut server = TestServer::launch();
    server.initialize(&root, Value::Null);
    let request_id = RequestId::from("cancelled-rename".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/rename",
        json!({
            "textDocument": {"uri": uri(&main)},
            "position": position_of(source, "badConst", 0),
            "newName": "BAD_CONST"
        }),
    );
    server.send_notification("$/cancelRequest", json!({"id": "cancelled-rename"}));
    let response = server.response(&request_id);
    let error = response.error.expect("cancelled rename must fail");
    assert_eq!(error.code, -32800);
    server.shutdown();
}

#[cfg(target_os = "linux")]
#[test]
fn rename_cancellation_during_final_content_hash_returns_request_canceled() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path().join("fixture");
    let provider = root.join("Provider.pas");
    let noise = root.join("Noise.pas");
    let provider_source =
        "unit Provider;\ninterface\nconst\n  badConst = 1;\nimplementation\nend.\n";
    write_file(&provider, provider_source);
    write_file(
        &noise,
        &format!(
            "unit Noise;\ninterface\nimplementation\n//{}\nend.\n",
            "x".repeat(15 * 1024 * 1024)
        ),
    );

    let watch_path = CString::new(noise.to_string_lossy().as_bytes()).expect("watch path");
    let fd = unsafe { inotify_init1(0) };
    assert!(fd >= 0, "inotify_init1 failed");
    let watch = unsafe { inotify_add_watch(fd, watch_path.as_ptr(), IN_OPEN) };
    assert!(watch >= 0, "inotify_add_watch failed");
    let (hash_started, hash_started_receiver) = mpsc::channel();
    let watcher = thread::spawn(move || {
        let mut file = unsafe { std::fs::File::from_raw_fd(fd) };
        let mut events = [0_u8; 4096];
        let mut opens = 0;
        while opens < 2 {
            let bytes = std::io::Read::read(&mut file, &mut events).expect("read inotify event");
            assert!(bytes > 0, "noise read must produce an open event");
            let mut offset = 0;
            while offset + 16 <= bytes {
                let mask = u32::from_ne_bytes(
                    events[offset + 4..offset + 8]
                        .try_into()
                        .expect("inotify mask bytes"),
                );
                let name_length = u32::from_ne_bytes(
                    events[offset + 12..offset + 16]
                        .try_into()
                        .expect("inotify name length bytes"),
                ) as usize;
                offset = offset.saturating_add(16).saturating_add(name_length);
                if mask & IN_OPEN != 0 {
                    opens += 1;
                }
            }
        }
        hash_started.send(()).expect("notify final hash start");
    });

    let mut server = TestServer::launch();
    server.initialize(&root, Value::Null);
    let request_id = RequestId::from("cancel-during-final-content-hash".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/rename",
        json!({
            "textDocument": {"uri": uri(&provider)},
            "position": position_of(provider_source, "badConst", 0),
            "newName": "BAD_CONST"
        }),
    );
    hash_started_receiver
        .recv_timeout(IO_TIMEOUT)
        .expect("final content hash must start");
    server.send_notification(
        "$/cancelRequest",
        json!({"id": "cancel-during-final-content-hash"}),
    );
    let response = server.response(&request_id);
    watcher.join().expect("watcher must finish");
    let error = response.error.expect("cancelled rename must fail");
    assert_eq!(error.code, -32800);
    assert_eq!(error.message, "request cancelled");
    server.shutdown();
}
