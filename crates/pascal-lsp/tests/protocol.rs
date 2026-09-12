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
use pascal_core::FileInfo;
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
        self.notification_with_timeout(method, IO_TIMEOUT)
    }

    fn notification_with_timeout(&mut self, method: &str, timeout: Duration) -> Value {
        if let Some(index) = self.pending.iter().position(|message| {
            matches!(message, Message::Notification(notification) if notification.method == method)
        }) {
            return match self.pending.remove(index).expect("pending notification") {
                Message::Notification(notification) => notification.params,
                _ => unreachable!("pending notification predicate"),
            };
        }
        let deadline = Instant::now() + timeout;
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

    fn initialize_with_watched_registration_and_relative_patterns(
        &mut self,
        root: &Path,
        initialization_options: Value,
        relative_pattern_support: bool,
    ) -> Value {
        self.initialize_with_client_capabilities_and_document_changes_and_relative(
            root,
            initialization_options,
            true,
            false,
            true,
            relative_pattern_support,
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
        self.initialize_with_client_capabilities_and_document_changes_and_relative(
            root,
            initialization_options,
            dynamic_watched_registration,
            action_support,
            document_changes,
            false,
        )
    }

    fn initialize_with_client_capabilities_and_document_changes_and_relative(
        &mut self,
        root: &Path,
        initialization_options: Value,
        dynamic_watched_registration: bool,
        action_support: bool,
        document_changes: bool,
        relative_pattern_support: bool,
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
                        "didChangeWatchedFiles": {
                            "dynamicRegistration": dynamic_watched_registration,
                            "relativePatternSupport": relative_pattern_support
                        },
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

fn diagnostics_for_uri(server: &mut TestServer, expected: &Url) -> Value {
    let expected = expected.to_string();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(index) = server.pending.iter().position(|message| {
            matches!(
                message,
                Message::Notification(notification)
                    if notification.method == "textDocument/publishDiagnostics"
                        && notification.params["uri"] == expected
            )
        }) {
            return match server.pending.remove(index).expect("pending diagnostics") {
                Message::Notification(notification) => notification.params,
                _ => unreachable!("pending diagnostics predicate"),
            };
        }

        match server.receive_until(deadline) {
            Message::Notification(notification)
                if notification.method == "textDocument/publishDiagnostics"
                    && notification.params["uri"] == expected =>
            {
                return notification.params;
            }
            other => server.pending.push_back(other),
        }
    }
}

struct SharedOwnerFixture {
    shared: PathBuf,
    a_main: PathBuf,
    a_project: PathBuf,
    a_config: PathBuf,
    b_main: PathBuf,
    b_project: PathBuf,
    main_source: String,
    shared_source: String,
}

fn shared_owner_fixture(root: &Path) -> SharedOwnerFixture {
    let shared = root.join("shared/Shared.pas");
    let a_main = root.join("A/Main.pas");
    let a_project = root.join("A/App.dproj");
    let a_config = root.join("A/lib/Config.pas");
    let b_main = root.join("B/Main.pas");
    let b_project = root.join("B/App.dproj");
    let b_config = root.join("B/lib/Config.pas");
    let main_source = "unit Main;\ninterface\nuses Shared;\nimplementation\nprocedure Use;\nbegin\n  Run;\nend;\nend.\n".to_string();
    let shared_source = "unit Shared;\ninterface\nuses Config;\nconst\n  BadConst = 1;\nprocedure Run;\nimplementation\nprocedure Run;\nbegin\n  ConfigRoutine;\nend;\nend.\n".to_string();
    let config_source = "unit Config;\ninterface\nprocedure ConfigRoutine;\nimplementation\nprocedure ConfigRoutine; begin end;\nend.\n";
    let project = "<Project><PropertyGroup><MainSource>Main.pas</MainSource></PropertyGroup><ItemGroup><DCCReference Include=\"../shared/Shared.pas\" /><DCCReference Include=\"lib/Config.pas\" /></ItemGroup></Project>";

    write_file(&shared, &shared_source);
    write_file(&a_main, &main_source);
    write_file(&a_project, project);
    write_file(&a_config, config_source);
    write_file(&b_main, &main_source);
    write_file(&b_project, project);
    write_file(&b_config, config_source);

    SharedOwnerFixture {
        shared,
        a_main,
        a_project,
        a_config,
        b_main,
        b_project,
        main_source,
        shared_source,
    }
}

struct OpenSharedOwnerFixture {
    shared: PathBuf,
    shared_source: String,
    main_source: String,
    a_main: PathBuf,
    a_project: PathBuf,
    a_config: PathBuf,
    b_main: PathBuf,
    b_project: PathBuf,
}

fn open_shared_owner_fixture(root: &Path) -> OpenSharedOwnerFixture {
    let shared = root.join("A/src/Shared.pas");
    let shared_source = "unit Shared;\ninterface\nuses Config;\nprocedure Run;\nimplementation\nprocedure Run;\nbegin\n  ConfigRoutine;\nend;\nend.\n".to_string();
    let main_source = "unit Main;\ninterface\nuses Shared;\nimplementation\nprocedure Use;\nbegin\n  Run;\nend;\nend.\n";
    let config_source = "unit Config;\ninterface\nprocedure ConfigRoutine;\nimplementation\nprocedure ConfigRoutine; begin end;\nend.\n";
    let a_main = root.join("A/Main.pas");
    let a_project = root.join("A/App.dproj");
    let a_config = root.join("A/lib/Config.pas");
    let b_main = root.join("B/Main.pas");
    let b_project = root.join("B/App.dproj");
    let b_config = root.join("B/lib/Config.pas");

    write_file(&shared, &shared_source);
    write_file(&a_main, main_source);
    write_file(&a_config, config_source);
    write_file(&b_main, main_source);
    write_file(&b_config, config_source);
    write_file(
        &a_project,
        "<Project><PropertyGroup><MainSource>Main.pas</MainSource></PropertyGroup><ItemGroup><DCCReference Include=\"src/Shared.pas\" /><DCCReference Include=\"lib/Config.pas\" /></ItemGroup></Project>",
    );
    write_file(
        &b_project,
        "<Project><PropertyGroup><MainSource>Main.pas</MainSource></PropertyGroup><ItemGroup><DCCReference Include=\"../A/src/Shared.pas\" /><DCCReference Include=\"lib/Config.pas\" /></ItemGroup></Project>",
    );

    OpenSharedOwnerFixture {
        shared,
        shared_source,
        main_source: main_source.to_string(),
        a_main,
        a_project,
        a_config,
        b_main,
        b_project,
    }
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
    assert_eq!(capabilities["experimental"]["projectSelection"], true);
    server.shutdown();
}

#[test]
fn project_context_reports_candidates_and_accepts_selection() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let main = root.join("Main.pas");
    write_file(&main, "unit Main; interface implementation end.");
    for name in ["A", "B"] {
        write_file(
            &root.join(format!("{name}.dproj")),
            "<Project><PropertyGroup><MainSource>Main.pas</MainSource></PropertyGroup></Project>",
        );
    }
    let mut server = TestServer::launch();
    server.initialize(root, Value::Null);
    let id = RequestId::from("project-list".to_string());
    server.send_request(
        id.clone(),
        "pascal/projectContext",
        json!({"textDocument": {"uri": uri(&main)}}),
    );
    let response = server.response(&id);
    assert!(response.error.is_none(), "{:?}", response.error);
    let context = response.result.unwrap();
    assert_eq!(context["candidates"].as_array().unwrap().len(), 2);
    assert_eq!(context["selectionMode"], "ambiguous");
    let id = RequestId::from("project-select".to_string());
    server.send_request(
        id.clone(),
        "pascal/selectProject",
        json!({
            "textDocument": {"uri": uri(&main)},
            "projectUri": uri(&root.join("B.dproj"))
        }),
    );
    let response = server.response(&id);
    assert!(response.error.is_none(), "{:?}", response.error);
    assert_eq!(
        response.result.unwrap()["selectedProjectUri"],
        uri(&root.join("B.dproj")).to_string()
    );
    let id = RequestId::from("project-reset".to_string());
    server.send_request(
        id.clone(),
        "pascal/selectProject",
        json!({
            "textDocument": {"uri": uri(&main)},
            "projectUri": null
        }),
    );
    let response = server.response(&id);
    assert!(response.error.is_none(), "{:?}", response.error);
    let context = response.result.unwrap();
    assert_eq!(context["selectionMode"], "ambiguous");
    assert!(context["selectedProjectUri"].is_null());
    server.shutdown();
}

#[test]
fn project_selection_rejects_missing_or_unrelated_projects() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let main = root.join("Main.pas");
    let candidate = root.join("A.dproj");
    write_file(&main, "unit Main; interface implementation end.");
    write_file(
        &candidate,
        "<Project><PropertyGroup><MainSource>Main.pas</MainSource></PropertyGroup></Project>",
    );
    let mut server = TestServer::launch();
    server.initialize(root, Value::Null);

    let missing_id = RequestId::from("project-missing".to_string());
    server.send_request(
        missing_id.clone(),
        "pascal/selectProject",
        json!({"textDocument": {"uri": uri(&main)}}),
    );
    assert_eq!(
        server
            .response(&missing_id)
            .error
            .expect("missing projectUri error")
            .code,
        -32602
    );

    let unrelated_id = RequestId::from("project-unrelated".to_string());
    server.send_request(
        unrelated_id.clone(),
        "pascal/selectProject",
        json!({
            "textDocument": {"uri": uri(&main)},
            "projectUri": uri(&root.join("Missing.dproj"))
        }),
    );
    assert_eq!(
        server
            .response(&unrelated_id)
            .error
            .expect("unrelated project error")
            .code,
        -32803
    );

    let non_file_id = RequestId::from("project-non-file".to_string());
    server.send_request(
        non_file_id.clone(),
        "pascal/selectProject",
        json!({
            "textDocument": {"uri": uri(&main)},
            "projectUri": "https://example.test/project.dproj"
        }),
    );
    assert_eq!(
        server
            .response(&non_file_id)
            .error
            .expect("non-file project error")
            .code,
        -32803
    );
    server.shutdown();
}

#[test]
fn project_context_preserves_configured_project_fallback() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let main = root.join("Main.pas");
    let configured = root.join("A.dproj");
    write_file(&main, "unit Main; interface implementation end.");
    for name in ["A", "B"] {
        write_file(
            &root.join(format!("{name}.dproj")),
            "<Project><PropertyGroup><MainSource>Main.pas</MainSource></PropertyGroup></Project>",
        );
    }

    let mut server = TestServer::launch();
    server.initialize(root, json!({"projectFile": "A.dproj"}));
    let id = RequestId::from("configured-project".to_string());
    server.send_request(
        id.clone(),
        "pascal/projectContext",
        json!({"textDocument": {"uri": uri(&main)}}),
    );
    let response = server.response(&id);
    assert!(response.error.is_none(), "{:?}", response.error);
    let context = response.result.unwrap();
    assert_eq!(context["selectionMode"], "configured");
    assert_eq!(context["selectedProjectUri"], uri(&configured).to_string());
    server.shutdown();
}

#[test]
fn removed_project_selection_is_reported_as_invalid() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let main = root.join("Main.pas");
    let selected = root.join("A.dproj");
    let remaining = root.join("B.dproj");
    write_file(&main, "unit Main; interface implementation end.");
    for project in [&selected, &remaining] {
        write_file(
            project,
            "<Project><PropertyGroup><MainSource>Main.pas</MainSource></PropertyGroup></Project>",
        );
    }

    let mut server = TestServer::launch();
    server.initialize(root, Value::Null);
    let select_id = RequestId::from("select-removed-project".to_string());
    server.send_request(
        select_id.clone(),
        "pascal/selectProject",
        json!({
            "textDocument": {"uri": uri(&main)},
            "projectUri": uri(&selected)
        }),
    );
    assert!(server.response(&select_id).error.is_none());
    fs::remove_file(&selected).unwrap();

    let context_id = RequestId::from("removed-project-context".to_string());
    server.send_request(
        context_id.clone(),
        "pascal/projectContext",
        json!({"textDocument": {"uri": uri(&main)}}),
    );
    let response = server.response(&context_id);
    assert!(response.error.is_none(), "{:?}", response.error);
    let context = response.result.unwrap();
    assert_eq!(context["selectionMode"], "invalid");
    assert!(context["selectedProjectUri"].is_null());
    server.shutdown();
}

#[test]
fn removed_project_selection_publishes_invalid_diagnostics_after_watcher_refresh() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let main = root.join("Main.pas");
    let selected = root.join("A.dproj");
    let remaining = root.join("B.dproj");
    let source = "unit Main; interface implementation end.";
    write_file(&main, source);
    for project in [&selected, &remaining] {
        write_file(
            project,
            "<Project><PropertyGroup><MainSource>Main.pas</MainSource></PropertyGroup></Project>",
        );
    }

    let mut server = TestServer::launch();
    server.initialize(root, Value::Null);
    server.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri(&main),
                "languageId": "pascal",
                "version": 1,
                "text": source
            }
        }),
    );
    let _ = server.notification("textDocument/publishDiagnostics");

    let select_id = RequestId::from("select-project-for-diagnostics".to_string());
    server.send_request(
        select_id.clone(),
        "pascal/selectProject",
        json!({
            "textDocument": {"uri": uri(&main)},
            "projectUri": uri(&selected)
        }),
    );
    assert!(server.response(&select_id).error.is_none());
    let _ = server.notification("textDocument/publishDiagnostics");

    fs::remove_file(&selected).unwrap();
    server.send_notification(
        "workspace/didChangeWatchedFiles",
        json!({"changes": [{"uri": uri(&selected), "type": 3}]}),
    );
    let diagnostics = server.notification("textDocument/publishDiagnostics");
    assert_eq!(diagnostics["uri"], uri(&main).to_string());
    assert_eq!(
        diagnostics["diagnostics"][0]["message"],
        "project selection is invalid; select a current project or Automatic"
    );
    server.shutdown();
}

#[test]
fn removed_nested_selection_does_not_fall_back_to_an_ancestor_project() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let nested = root.join("nested");
    let main = nested.join("Main.pas");
    let ancestor_project = root.join("Ancestor.dproj");
    let selected_project = nested.join("Selected.dproj");
    write_file(&main, "unit Main; interface implementation end.");
    write_file(
        &ancestor_project,
        "<Project><PropertyGroup><MainSource>Main.pas</MainSource></PropertyGroup></Project>",
    );
    write_file(
        &selected_project,
        "<Project><PropertyGroup><MainSource>Main.pas</MainSource></PropertyGroup></Project>",
    );

    let mut server = TestServer::launch();
    server.initialize(root, Value::Null);
    let select_id = RequestId::from("select-nested-project".to_string());
    server.send_request(
        select_id.clone(),
        "pascal/selectProject",
        json!({
            "textDocument": {"uri": uri(&main)},
            "projectUri": uri(&selected_project)
        }),
    );
    assert!(server.response(&select_id).error.is_none());
    fs::remove_file(&selected_project).unwrap();

    let context_id = RequestId::from("removed-nested-project-context".to_string());
    server.send_request(
        context_id.clone(),
        "pascal/projectContext",
        json!({"textDocument": {"uri": uri(&main)}}),
    );
    let response = server.response(&context_id);
    assert!(response.error.is_none(), "{response:?}");
    let context = response.result.unwrap();
    assert_eq!(context["selectionMode"], "invalid");
    assert!(context["selectedProjectUri"].is_null());
    server.shutdown();
}

#[test]
fn automatic_reset_clears_removed_nested_override_without_removing_ancestor_choice() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let nested = root.join("nested");
    let ancestor_main = root.join("Main.pas");
    let nested_main = nested.join("Main.pas");
    let ancestor_project = root.join("Ancestor.dproj");
    let nested_project = nested.join("Nested.dproj");
    write_file(&ancestor_main, "unit Main; interface implementation end.");
    write_file(&nested_main, "unit Main; interface implementation end.");
    write_file(
        &ancestor_project,
        "<Project><PropertyGroup><MainSource>Main.pas</MainSource></PropertyGroup></Project>",
    );
    write_file(
        &nested_project,
        "<Project><PropertyGroup><MainSource>Main.pas</MainSource></PropertyGroup></Project>",
    );

    let mut server = TestServer::launch();
    server.initialize(root, Value::Null);
    for (id, document, project) in [
        ("ancestor-choice", &ancestor_main, &ancestor_project),
        ("nested-choice", &nested_main, &nested_project),
    ] {
        let id = RequestId::from(id.to_string());
        server.send_request(
            id.clone(),
            "pascal/selectProject",
            json!({
                "textDocument": {"uri": uri(document)},
                "projectUri": uri(project)
            }),
        );
        assert!(server.response(&id).error.is_none());
    }

    fs::remove_file(&nested_project).unwrap();
    server.send_notification(
        "workspace/didChangeWatchedFiles",
        json!({"changes": [{"uri": uri(&nested_project), "type": 3}]}),
    );
    let invalid_id = RequestId::from("nested-invalid-before-reset".to_string());
    server.send_request(
        invalid_id.clone(),
        "pascal/projectContext",
        json!({"textDocument": {"uri": uri(&nested_main)}}),
    );
    let invalid = server.response(&invalid_id);
    assert_eq!(invalid.result.unwrap()["selectionMode"], "invalid");

    let reset_id = RequestId::from("nested-automatic-reset".to_string());
    server.send_request(
        reset_id.clone(),
        "pascal/selectProject",
        json!({
            "textDocument": {"uri": uri(&nested_main)},
            "projectUri": null
        }),
    );
    let reset = server.response(&reset_id);
    assert!(reset.error.is_none(), "{reset:?}");
    let context = reset.result.unwrap();
    assert_eq!(context["selectionMode"], "directory");
    assert_eq!(
        context["selectedProjectUri"],
        uri(&ancestor_project).to_string()
    );

    let ancestor_id = RequestId::from("ancestor-choice-after-reset".to_string());
    server.send_request(
        ancestor_id.clone(),
        "pascal/projectContext",
        json!({"textDocument": {"uri": uri(&ancestor_main)}}),
    );
    assert_eq!(
        server.response(&ancestor_id).result.unwrap()["selectedProjectUri"],
        uri(&ancestor_project).to_string()
    );
    server.shutdown();
}

#[test]
fn project_selections_are_independent_and_nested_scopes_do_not_inherit() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let outer_main = root.join("Main.pas");
    let nested_main = root.join("nested/Main.pas");
    write_file(&outer_main, "unit Main; interface implementation end.");
    write_file(&nested_main, "unit Main; interface implementation end.");
    for name in ["OuterA", "OuterB"] {
        write_file(
            &root.join(format!("{name}.dproj")),
            "<Project><PropertyGroup><MainSource>Main.pas</MainSource></PropertyGroup></Project>",
        );
    }
    for name in ["NestedA", "NestedB"] {
        write_file(
            &root.join("nested").join(format!("{name}.dproj")),
            "<Project><PropertyGroup><MainSource>Main.pas</MainSource></PropertyGroup></Project>",
        );
    }

    let mut server = TestServer::launch();
    server.initialize(root, Value::Null);
    for (id, document, project) in [
        ("outer-select", &outer_main, root.join("OuterA.dproj")),
        (
            "nested-select",
            &nested_main,
            root.join("nested/NestedB.dproj"),
        ),
    ] {
        let id = RequestId::from(id.to_string());
        server.send_request(
            id.clone(),
            "pascal/selectProject",
            json!({
                "textDocument": {"uri": uri(document)},
                "projectUri": uri(&project)
            }),
        );
        let response = server.response(&id);
        assert!(response.error.is_none(), "{response:?}");
        assert_eq!(response.result.unwrap()["selectionMode"], "directory");
    }

    for (id, document, project) in [
        ("outer-context", &outer_main, root.join("OuterA.dproj")),
        (
            "nested-context",
            &nested_main,
            root.join("nested/NestedB.dproj"),
        ),
    ] {
        let id = RequestId::from(id.to_string());
        server.send_request(
            id.clone(),
            "pascal/projectContext",
            json!({"textDocument": {"uri": uri(document)}}),
        );
        let response = server.response(&id);
        assert!(response.error.is_none(), "{response:?}");
        assert_eq!(
            response.result.unwrap()["selectedProjectUri"],
            uri(&project).to_string()
        );
    }
    server.shutdown();
}

#[test]
fn project_context_reports_project_scoped_configuration_paths() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let project = root.join("app");
    let main = project.join("Main.pas");
    let lint_config = project.join(".lint4d.toml");
    let fmt_config = project.join(".fmt4d.toml");
    write_file(&main, "unit Main; interface implementation end.");
    write_file(
        &project.join("App.dproj"),
        "<Project><PropertyGroup><MainSource>Main.pas</MainSource></PropertyGroup></Project>",
    );
    write_file(
        &lint_config,
        "[rules.naming]\nconstant_style = \"PascalCase\"\n",
    );
    write_file(&fmt_config, "[format]\nindent_size = 4\n");

    let mut server = TestServer::launch();
    server.initialize(root, Value::Null);
    let id = RequestId::from("project-config".to_string());
    server.send_request(
        id.clone(),
        "pascal/projectContext",
        json!({"textDocument": {"uri": uri(&main)}}),
    );
    let response = server.response(&id);
    assert!(response.error.is_none(), "{response:?}");
    let context = response.result.unwrap();
    assert_eq!(context["lintConfigUri"], uri(&lint_config).to_string());
    assert_eq!(context["fmtConfigUri"], uri(&fmt_config).to_string());
    server.shutdown();
}

#[test]
fn project_sidecar_configuration_applies_to_shared_source() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let app = root.join("app");
    let shared = root.join("shared/Shared.pas");
    let source = "unit Shared;\ninterface\nconst\n  GoodConst = 1;\nimplementation\nprocedure Run;\nbegin\n  Log(GoodConst);\nend;\nend.\n";
    write_file(&shared, source);
    write_file(
        &app.join("App.dproj"),
        "<Project><PropertyGroup><MainSource>App.dpr</MainSource></PropertyGroup></Project>",
    );
    write_file(&app.join("App.dpr"), "program App; begin end.\n");
    write_file(&root.join(".fmt4d.toml"), "[format]\nindent_size = 2\n");
    write_file(&app.join(".fmt4d.toml"), "[format]\nindent_size = 4\n");
    write_file(
        &root.join(".lint4d.toml"),
        "[rules.naming]\nconstant_style = \"PascalCase\"\n",
    );
    write_file(
        &app.join(".lint4d.toml"),
        "[rules.naming]\nconstant_style = \"UPPER_CASE\"\n",
    );

    let mut server = TestServer::launch();
    server.initialize_with_action_support(root, json!({"projectFile": "app/App.dproj"}));
    server.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri(&shared),
                "languageId": "pascal",
                "version": 1,
                "text": source
            }
        }),
    );
    let diagnostics = server.notification("textDocument/publishDiagnostics");
    assert!(
        diagnostics["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|diagnostic| diagnostic["code"] == "constant-naming"),
        "the selected project lint sidecar must be used: {diagnostics}"
    );

    let format_id = RequestId::from("format-project-sidecar".to_string());
    server.send_request(
        format_id.clone(),
        "textDocument/formatting",
        json!({
            "textDocument": {"uri": uri(&shared)},
            "options": {"tabSize": 2, "insertSpaces": true}
        }),
    );
    let response = server.response(&format_id);
    assert!(response.error.is_none(), "{:?}", response.error);
    let edits = response.result.unwrap();
    assert!(edits[0]["newText"].as_str().unwrap().contains("\n    Log"));

    let start = position_of(source, "GoodConst", 0);
    let end = Position::new(start.line, start.character + 9);
    let action_id = RequestId::from("project-sidecar-action".to_string());
    server.send_request(
        action_id.clone(),
        "textDocument/codeAction",
        json!({
            "textDocument": {"uri": uri(&shared)},
            "range": {"start": start, "end": end},
            "context": {
                "diagnostics": [{
                    "range": {"start": start, "end": end},
                    "severity": 2,
                    "code": "constant-naming",
                    "source": "lint4d",
                    "message": "naming violation"
                }],
                "only": ["quickfix"]
            }
        }),
    );
    let response = server.response(&action_id);
    assert!(response.error.is_none(), "{response:?}");
    let actions = response.result.unwrap();
    assert_eq!(actions[0]["title"], "Rename 'GoodConst' to 'GOOD_CONST'");
    assert!(actions[0]["edit"].is_null());

    let resolve_id = RequestId::from("format-project-sidecar-resolve".to_string());
    server.send_request(resolve_id.clone(), "codeAction/resolve", actions[0].clone());
    let resolved = server.response(&resolve_id);
    assert!(resolved.error.is_none(), "{resolved:?}");
    let resolved_action = resolved.result.unwrap();
    assert_eq!(
        resolved_action["title"],
        "Rename 'GoodConst' to 'GOOD_CONST'"
    );
    assert!(resolved_action["edit"].is_object());
    assert!(resolved_action["edit"].to_string().contains("GOOD_CONST"));
    server.shutdown();
}

#[test]
fn project_sidecar_exclude_suppresses_diagnostics_and_naming_actions() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let app = root.join("app");
    let hidden = app.join("excluded/Hidden.pas");
    let source = "unit Hidden;\ninterface\nconst\n  GoodConst = 1;\nimplementation\nend.\n";
    write_file(&hidden, source);
    write_file(
        &app.join("App.dproj"),
        "<Project><PropertyGroup><MainSource>App.dpr</MainSource></PropertyGroup></Project>",
    );
    write_file(&app.join("App.dpr"), "program App; begin end.\n");
    write_file(
        &app.join(".lint4d.toml"),
        "[lint4d]\nexclude = [\"excluded/**\"]\n[rules.naming]\nconstant_style = \"UPPER_CASE\"\n",
    );

    let mut server = TestServer::launch();
    server.initialize(root, json!({"projectFile": "app/App.dproj"}));
    server.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri(&hidden),
                "languageId": "pascal",
                "version": 1,
                "text": source
            }
        }),
    );
    let diagnostics = server.notification("textDocument/publishDiagnostics");
    assert!(
        diagnostics["diagnostics"].as_array().unwrap().is_empty(),
        "project-relative lint excludes must clear diagnostics: {diagnostics}"
    );

    let start = position_of(source, "GoodConst", 0);
    let end = Position::new(start.line, start.character + 9);
    let action_id = RequestId::from("project-sidecar-exclude-action".to_string());
    server.send_request(
        action_id.clone(),
        "textDocument/codeAction",
        json!({
            "textDocument": {"uri": uri(&hidden)},
            "range": {"start": start, "end": end},
            "context": {
                "diagnostics": [{
                    "range": {"start": start, "end": end},
                    "severity": 2,
                    "code": "constant-naming",
                    "source": "lint4d",
                    "message": "naming violation"
                }],
                "only": ["quickfix"]
            }
        }),
    );
    let response = server.response(&action_id);
    assert!(response.error.is_none(), "{response:?}");
    assert!(response.result.unwrap().as_array().unwrap().is_empty());
    server.shutdown();
}

#[test]
fn project_sidecar_sibling_exclude_is_relative_to_the_sidecar() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let app = root.join("app");
    let shared = root.join("shared/Shared.pas");
    let source = "unit Shared;\ninterface\nconst\n  BadConst = 1;\nimplementation\nend.\n";
    write_file(&shared, source);
    write_file(
        &app.join("App.dproj"),
        "<Project><PropertyGroup><MainSource>App.dpr</MainSource></PropertyGroup></Project>",
    );
    write_file(&app.join("App.dpr"), "program App; begin end.\n");
    write_file(
        &app.join(".lint4d.toml"),
        "[lint4d]\nexclude = [\"../shared/*.pas\"]\n[rules.naming]\nconstant_style = \"UPPER_CASE\"\n",
    );

    let mut server = TestServer::launch();
    server.initialize(root, json!({"projectFile": "app/App.dproj"}));
    server.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri(&shared),
                "languageId": "pascal",
                "version": 1,
                "text": source
            }
        }),
    );
    let diagnostics = server.notification("textDocument/publishDiagnostics");
    assert!(
        diagnostics["diagnostics"].as_array().unwrap().is_empty(),
        "sidecar-relative parent excludes must clear diagnostics: {diagnostics}"
    );

    let position = position_of(source, "BadConst", 0);
    let action_id = RequestId::from("project-sidecar-sibling-exclude-action".to_string());
    server.send_request(
        action_id.clone(),
        "textDocument/codeAction",
        json!({
            "textDocument": {"uri": uri(&shared)},
            "range": {"start": position, "end": position},
            "context": {
                "diagnostics": [{
                    "range": {"start": position, "end": position},
                    "severity": 2,
                    "code": "constant-naming",
                    "source": "lint4d",
                    "message": "naming violation"
                }],
                "only": ["quickfix"]
            }
        }),
    );
    let response = server.response(&action_id);
    assert!(response.error.is_none(), "{response:?}");
    assert!(response.result.unwrap().as_array().unwrap().is_empty());
    server.shutdown();
}

#[test]
fn project_sidecar_external_grouping_uses_project_unit_precedence() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let app = root.join("app");
    let main = app.join("Main.pas");
    let project_unit = app.join("src/Shared.pas");
    let external_unit = app.join("vendor/Shared.pas");
    let vendor_only = app.join("vendor/ThirdParty.pas");
    let source = "unit Main;\ninterface\nuses\n  ThirdParty,\n  Shared,\n  System.SysUtils;\nimplementation\nend.\n";
    let fmt_toml = "[format.uses]\nsort = true\ngroup = true\nexternal_paths = [\"vendor\"]\nexternal_prefixes = [\"Spring\"]\n";
    write_file(&main, source);
    write_file(
        &project_unit,
        "unit Shared; interface implementation end.\n",
    );
    write_file(
        &external_unit,
        "unit Shared; interface implementation end.\n",
    );
    write_file(
        &vendor_only,
        "unit ThirdParty; interface implementation end.\n",
    );
    write_file(
        &app.join("App.dproj"),
        "<Project><PropertyGroup><MainSource>Main.pas</MainSource></PropertyGroup><ItemGroup><DCCReference Include=\"src/Shared.pas\" /></ItemGroup></Project>",
    );
    write_file(&app.join("App.dpr"), "program App; begin end.\n");
    write_file(&app.join(".fmt4d.toml"), fmt_toml);

    let mut expected_config = fmt4d::config::FmtConfig::from_toml(fmt_toml).unwrap();
    expected_config.project_root = Some(app.clone());
    let expected_external = HashSet::from(["thirdparty".to_string()]);
    let expected = fmt4d::format_source(
        source.as_bytes(),
        &FileInfo::new(main.clone()),
        &expected_config,
        &expected_external,
    )
    .unwrap();

    let mut server = TestServer::launch();
    server.initialize(root, json!({"projectFile": "app/App.dproj"}));
    let id = RequestId::from("format-project-sidecar-external".to_string());
    server.send_request(
        id.clone(),
        "textDocument/formatting",
        json!({
            "textDocument": {"uri": uri(&main)},
            "options": {"tabSize": 2, "insertSpaces": true}
        }),
    );
    let response = server.response(&id);
    assert!(response.error.is_none(), "{response:?}");
    assert_eq!(response.result.unwrap()[0]["newText"], expected);
    server.shutdown();
}

#[test]
fn project_sidecar_external_grouping_respects_cli_project_collection() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let app = root.join("app");
    let main = app.join("src/Main.pas");
    let source_path = app.join("Uses.pas");
    let project_unit = app.join("src/Shared.pas");
    let external_same_name = app.join("vendor/Shared.pas");
    let external_main = app.join("vendor/Main.pas");
    let external_only = app.join("vendor/ThirdParty.pas");
    let main_source = "unit Main;\ninterface\nimplementation\nend.\n";
    let source = "unit Uses;\ninterface\nuses\n  ThirdParty,\n  Shared,\n  Main,\n  Spring.Logging;\nimplementation\nend.\n";
    let fmt_toml = "[format.uses]\nsort = true\ngroup = true\nexternal_paths = [\"vendor\"]\nexternal_prefixes = [\"Spring\"]\n";
    write_file(&main, main_source);
    write_file(&source_path, source);
    write_file(
        &project_unit,
        "unit Shared; interface implementation end.\n",
    );
    write_file(
        &external_same_name,
        "unit Shared; interface implementation end.\n",
    );
    write_file(&external_main, "unit Main; interface implementation end.\n");
    write_file(
        &external_only,
        "unit ThirdParty; interface implementation end.\n",
    );
    write_file(
        &app.join("App.dproj"),
        "<Project><PropertyGroup><MainSource>src/Main.pas</MainSource><DCC_UnitSearchPath>src;vendor</DCC_UnitSearchPath></PropertyGroup><ItemGroup><DCCReference Include=\"src/Shared.pas\" /></ItemGroup></Project>",
    );
    write_file(&app.join("App.dpr"), "program App; begin end.\n");
    write_file(&app.join(".fmt4d.toml"), fmt_toml);

    let mut expected_config = fmt4d::config::FmtConfig::from_toml(fmt_toml).unwrap();
    expected_config.project_root = Some(app.clone());
    let expected_external = HashSet::from(["thirdparty".to_string()]);
    let expected = fmt4d::format_source(
        source.as_bytes(),
        &FileInfo::new(source_path.clone()),
        &expected_config,
        &expected_external,
    )
    .unwrap();

    let mut server = TestServer::launch();
    server.initialize(root, json!({"projectFile": "app/App.dproj"}));
    let id = RequestId::from("format-project-sidecar-cli-collection".to_string());
    server.send_request(
        id.clone(),
        "textDocument/formatting",
        json!({
            "textDocument": {"uri": uri(&source_path)},
            "options": {"tabSize": 2, "insertSpaces": true}
        }),
    );
    let response = server.response(&id);
    assert!(response.error.is_none(), "{response:?}");
    assert_eq!(response.result.unwrap()[0]["newText"], expected);
    server.shutdown();
}

#[test]
fn project_sidecar_external_scan_respects_workspace_bounds() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let app = root.join("app");
    let main = app.join("Main.pas");
    let source =
        "unit Main;\ninterface\nuses\n  ThirdParty,\n  System.SysUtils;\nimplementation\nend.\n";
    write_file(&main, source);
    write_file(
        &app.join("App.dproj"),
        "<Project><PropertyGroup><MainSource>Main.pas</MainSource></PropertyGroup></Project>",
    );
    write_file(&app.join("App.dpr"), "program App; begin end.\n");
    write_file(
        &app.join(".fmt4d.toml"),
        "[format.uses]\ngroup = true\nexternal_paths = [\"vendor\"]\n",
    );
    for name in ["ExternalUnit", "OtherOne", "OtherTwo"] {
        write_file(
            &app.join("vendor").join(format!("{name}.pas")),
            &format!("unit {name}; interface implementation end.\n"),
        );
    }

    let mut server = TestServer::launch();
    server.initialize(root, json!({"projectFile": "app/App.dproj", "maxFiles": 2}));
    let id = RequestId::from("format-project-sidecar-bound".to_string());
    server.send_request(
        id.clone(),
        "textDocument/formatting",
        json!({
            "textDocument": {"uri": uri(&main)},
            "options": {"tabSize": 2, "insertSpaces": true}
        }),
    );
    let response = server.response(&id);
    assert!(
        response.error.is_some(),
        "external scan must stop at its bound: {response:?}"
    );
    let error = response.error.expect("external scan error");
    assert!(error.message.contains("external"));
    assert!(error.message.contains("limit"));
    server.shutdown();
}

#[test]
fn project_sidecar_missing_external_path_refuses_formatting() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let app = root.join("app");
    let main = app.join("Main.pas");
    let missing = app.join("missing");
    let source = "unit Main;\ninterface\nuses\n  System.SysUtils;\nimplementation\nend.\n";
    write_file(&main, source);
    write_file(
        &app.join("App.dproj"),
        "<Project><PropertyGroup><MainSource>Main.pas</MainSource></PropertyGroup></Project>",
    );
    write_file(&app.join("App.dpr"), "program App; begin end.\n");
    write_file(
        &app.join(".fmt4d.toml"),
        "[format.uses]\ngroup = true\nexternal_paths = [\"missing\"]\n",
    );

    let mut server = TestServer::launch();
    server.initialize(root, json!({"projectFile": "app/App.dproj"}));
    let id = RequestId::from("format-project-sidecar-missing-external".to_string());
    server.send_request(
        id.clone(),
        "textDocument/formatting",
        json!({
            "textDocument": {"uri": uri(&main)},
            "options": {"tabSize": 2, "insertSpaces": true}
        }),
    );
    let response = server.response(&id);
    let error = response.error.expect("missing external path must fail");
    assert!(error.message.contains("external path"));
    assert!(error.message.contains(&missing.display().to_string()));
    server.shutdown();
}

#[test]
fn project_sidecar_non_directory_external_path_refuses_formatting() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let app = root.join("app");
    let main = app.join("Main.pas");
    let invalid_path = app.join("NotADirectory.pas");
    let source = "unit Main;\ninterface\nuses\n  System.SysUtils;\nimplementation\nend.\n";
    write_file(&main, source);
    write_file(
        &invalid_path,
        "unit NotADirectory; interface implementation end.\n",
    );
    write_file(
        &app.join("App.dproj"),
        "<Project><PropertyGroup><MainSource>Main.pas</MainSource></PropertyGroup></Project>",
    );
    write_file(&app.join("App.dpr"), "program App; begin end.\n");
    write_file(
        &app.join(".fmt4d.toml"),
        "[format.uses]\ngroup = true\nexternal_paths = [\"NotADirectory.pas\"]\n",
    );

    let mut server = TestServer::launch();
    server.initialize(root, json!({"projectFile": "app/App.dproj"}));
    let id = RequestId::from("format-project-sidecar-invalid-external".to_string());
    server.send_request(
        id.clone(),
        "textDocument/formatting",
        json!({
            "textDocument": {"uri": uri(&main)},
            "options": {"tabSize": 2, "insertSpaces": true}
        }),
    );
    let response = server.response(&id);
    let error = response
        .error
        .expect("non-directory external path must fail");
    assert!(error.message.contains("is not a directory"));
    assert!(error.message.contains(&invalid_path.display().to_string()));
    server.shutdown();
}

#[cfg(unix)]
#[test]
fn project_sidecar_permission_denied_external_directory_refuses_formatting() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let app = root.join("app");
    let main = app.join("Main.pas");
    let external = app.join("vendor");
    let source = "unit Main;\ninterface\nuses\n  System.SysUtils;\nimplementation\nend.\n";
    write_file(&main, source);
    write_file(
        &app.join("App.dproj"),
        "<Project><PropertyGroup><MainSource>Main.pas</MainSource></PropertyGroup></Project>",
    );
    write_file(&app.join("App.dpr"), "program App; begin end.\n");
    write_file(
        &app.join(".fmt4d.toml"),
        "[format.uses]\ngroup = true\nexternal_paths = [\"vendor\"]\n",
    );
    fs::create_dir_all(&external).unwrap();
    let original_mode = fs::metadata(&external).unwrap().permissions().mode();
    fs::set_permissions(&external, fs::Permissions::from_mode(0o000)).unwrap();

    if fs::read_dir(&external).is_ok() {
        fs::set_permissions(&external, fs::Permissions::from_mode(original_mode)).unwrap();
        eprintln!(
            "skipping permission-denied external-directory test: runner can enumerate mode-000 directories"
        );
        return;
    }

    let mut server = TestServer::launch();
    server.initialize(root, json!({"projectFile": "app/App.dproj"}));
    let id = RequestId::from("format-project-sidecar-permission-denied".to_string());
    server.send_request(
        id.clone(),
        "textDocument/formatting",
        json!({
            "textDocument": {"uri": uri(&main)},
            "options": {"tabSize": 2, "insertSpaces": true}
        }),
    );
    let response = server.response(&id);
    let error = response
        .error
        .expect("permission-denied external path must fail");
    assert!(
        response.result.is_none(),
        "permission failure must not return an edit"
    );
    assert!(error.message.contains("scan"));
    assert!(error.message.contains(&external.display().to_string()));

    fs::set_permissions(&external, fs::Permissions::from_mode(original_mode)).unwrap();
    server.shutdown();
}

#[test]
fn project_sidecar_external_scan_enforces_per_file_bytes() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let app = root.join("app");
    let main = app.join("Main.pas");
    let external = app.join("vendor/TooBig.pas");
    let source = "unit Main;\ninterface\nuses\n  System.SysUtils;\nimplementation\nend.\n";
    write_file(&main, source);
    write_file(
        &external,
        &format!(
            "unit TooBig; interface implementation end. // {}\n",
            "x".repeat(200)
        ),
    );
    write_file(
        &app.join("App.dproj"),
        "<Project><PropertyGroup><MainSource>Main.pas</MainSource></PropertyGroup></Project>",
    );
    write_file(&app.join("App.dpr"), "program App; begin end.\n");
    write_file(
        &app.join(".fmt4d.toml"),
        "[format.uses]\ngroup = true\nexternal_paths = [\"vendor\"]\n",
    );

    let mut server = TestServer::launch();
    server.initialize(
        root,
        json!({"projectFile": "app/App.dproj", "maxFileBytes": 128}),
    );
    let id = RequestId::from("format-project-sidecar-per-file-limit".to_string());
    server.send_request(
        id.clone(),
        "textDocument/formatting",
        json!({
            "textDocument": {"uri": uri(&main)},
            "options": {"tabSize": 2, "insertSpaces": true}
        }),
    );
    let response = server.response(&id);
    let error = response.error.expect("oversized external file must fail");
    assert!(error.message.contains("per-file limit"));
    assert!(error.message.contains(&external.display().to_string()));
    server.shutdown();
}

#[test]
fn project_sidecar_external_scan_enforces_total_bytes() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let app = root.join("app");
    let main = app.join("Main.pas");
    let first = app.join("vendor/First.pas");
    let second = app.join("vendor/Second.pas");
    let source = "unit Main;\ninterface\nuses\n  System.SysUtils;\nimplementation\nend.\n";
    write_file(&main, source);
    write_file(&first, "unit First; interface implementation end.\n");
    write_file(&second, "unit Second; interface implementation end.\n");
    write_file(
        &app.join("App.dproj"),
        "<Project><PropertyGroup><MainSource>Main.pas</MainSource></PropertyGroup></Project>",
    );
    write_file(&app.join("App.dpr"), "program App; begin end.\n");
    write_file(
        &app.join(".fmt4d.toml"),
        "[format.uses]\ngroup = true\nexternal_paths = [\"vendor\"]\n",
    );

    let mut server = TestServer::launch();
    server.initialize(
        root,
        json!({"projectFile": "app/App.dproj", "maxTotalBytes": 64}),
    );
    let id = RequestId::from("format-project-sidecar-total-byte-limit".to_string());
    server.send_request(
        id.clone(),
        "textDocument/formatting",
        json!({
            "textDocument": {"uri": uri(&main)},
            "options": {"tabSize": 2, "insertSpaces": true}
        }),
    );
    let response = server.response(&id);
    let error = response.error.expect("external byte budget must fail");
    assert!(error.message.contains("byte limit"));
    server.shutdown();
}

#[test]
fn project_sidecar_lint_rules_styles_and_suppressions_are_consistent() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let main = root.join("Main.pas");
    let source = "unit Main;\ninterface\nconst\n  // lint4d:ignore-next-line constant-naming\n  badConst = 1;\n  anotherConst = 2;\nimplementation\nprocedure Run;\nvar\n  BadLocal: Integer;\nbegin\n  BadLocal := anotherConst;\nend;\nend.\n";
    write_file(&main, source);
    write_file(
        &root.join(".lint4d.toml"),
        "[rules]\nconstant-naming = \"warning\"\nlocal-variable-naming = \"warning\"\n[rules.naming]\nconstant_style = \"PascalCase\"\nlocal_variable_style = \"camelCase\"\n",
    );

    let mut server = TestServer::launch();
    server.initialize(root, Value::Null);
    server.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri(&main),
                "languageId": "pascal",
                "version": 1,
                "text": source
            }
        }),
    );
    let diagnostics = server.notification("textDocument/publishDiagnostics");
    let diagnostics = diagnostics["diagnostics"].as_array().unwrap();
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic["code"] == "constant-naming"
            && diagnostic["severity"] == 2
            && diagnostic["message"]
                .as_str()
                .unwrap()
                .contains("anotherConst")
    }));
    assert!(!diagnostics.iter().any(|diagnostic| {
        diagnostic["message"]
            .as_str()
            .unwrap_or_default()
            .contains("badConst")
    }));
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic["code"] == "local-variable-naming"
            && diagnostic["severity"] == 2
            && diagnostic["message"].as_str().unwrap().contains("BadLocal")
    }));

    let start = position_of(source, "anotherConst", 0);
    let end = Position::new(start.line, start.character + 12);
    let action_id = RequestId::from("project-sidecar-style-action".to_string());
    server.send_request(
        action_id.clone(),
        "textDocument/codeAction",
        json!({
            "textDocument": {"uri": uri(&main)},
            "range": {"start": start, "end": end},
            "context": {
                "diagnostics": [{
                    "range": {"start": start, "end": end},
                    "severity": 2,
                    "code": "constant-naming",
                    "source": "lint4d",
                    "message": "naming violation"
                }],
                "only": ["quickfix"]
            }
        }),
    );
    let response = server.response(&action_id);
    assert!(response.error.is_none(), "{response:?}");
    assert_eq!(
        response.result.unwrap()[0]["title"],
        "Rename 'anotherConst' to 'AnotherConst'"
    );

    let local_start = position_of(source, "BadLocal", 0);
    let local_end = Position::new(local_start.line, local_start.character + 8);
    let local_action_id = RequestId::from("project-sidecar-local-style-action".to_string());
    server.send_request(
        local_action_id.clone(),
        "textDocument/codeAction",
        json!({
            "textDocument": {"uri": uri(&main)},
            "range": {"start": local_start, "end": local_end},
            "context": {
                "diagnostics": [{
                    "range": {"start": local_start, "end": local_end},
                    "severity": 2,
                    "code": "local-variable-naming",
                    "source": "lint4d",
                    "message": "naming violation"
                }],
                "only": ["quickfix"]
            }
        }),
    );
    let local_response = server.response(&local_action_id);
    assert!(local_response.error.is_none(), "{local_response:?}");
    assert_eq!(
        local_response.result.unwrap()[0]["title"],
        "Rename 'BadLocal' to 'badLocal'"
    );
    server.shutdown();
}

#[test]
fn project_sidecar_selected_shared_source_uses_local_variable_style() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let app = root.join("app");
    let shared = root.join("shared/Shared.pas");
    let source = "unit Shared;\ninterface\nprocedure Run;\nimplementation\nprocedure Run;\nvar\n  BadLocal: Integer;\nbegin\n  BadLocal := 1;\nend;\nend.\n";
    write_file(&shared, source);
    write_file(
        &app.join("App.dproj"),
        "<Project><PropertyGroup><MainSource>Main.pas</MainSource></PropertyGroup><ItemGroup><DCCReference Include=\"../shared/Shared.pas\" /></ItemGroup></Project>",
    );
    write_file(&app.join("Main.dpr"), "program App; begin end.\n");
    write_file(
        &root.join(".lint4d.toml"),
        "[rules]\nlocal-variable-naming = \"warning\"\n[rules.naming]\nlocal_variable_style = \"PascalCase\"\n",
    );
    write_file(
        &app.join(".lint4d.toml"),
        "[rules]\nlocal-variable-naming = \"warning\"\n[rules.naming]\nlocal_variable_style = \"camelCase\"\n",
    );

    let mut server = TestServer::launch();
    server.initialize_with_action_support(root, json!({"projectFile": "app/App.dproj"}));
    server.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri(&shared),
                "languageId": "pascal",
                "version": 1,
                "text": source
            }
        }),
    );
    let diagnostics = server.notification("textDocument/publishDiagnostics");
    let diagnostics = diagnostics["diagnostics"].as_array().unwrap();
    let position = position_of(source, "BadLocal", 0);
    let end = Position::new(position.line, position.character + 8);
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic["code"] == "local-variable-naming"
            && diagnostic["message"]
                .as_str()
                .unwrap_or_default()
                .contains("BadLocal")
    }));

    let action_id = RequestId::from("project-sidecar-selected-local-style".to_string());
    server.send_request(
        action_id.clone(),
        "textDocument/codeAction",
        json!({
            "textDocument": {"uri": uri(&shared)},
            "range": {"start": position, "end": end},
            "context": {
                "diagnostics": [{
                    "range": {"start": position, "end": end},
                    "severity": 2,
                    "code": "local-variable-naming",
                    "source": "lint4d",
                    "message": "naming violation"
                }],
                "only": ["quickfix"]
            }
        }),
    );
    let response = server.response(&action_id);
    assert!(response.error.is_none(), "{response:?}");
    assert_eq!(
        response.result.unwrap()[0]["title"],
        "Rename 'BadLocal' to 'badLocal'"
    );
    server.shutdown();
}

#[test]
fn project_sidecar_malformed_fmt_refuses_formatting_but_keeps_lint_fallback() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let app = root.join("app");
    let main = app.join("Main.pas");
    let source = "unit Main;\ninterface\nconst\n  badConst = 1;\nimplementation\nend.\n";
    write_file(&main, source);
    write_file(
        &app.join("App.dproj"),
        "<Project><PropertyGroup><MainSource>Main.pas</MainSource></PropertyGroup></Project>",
    );
    write_file(&app.join("App.dpr"), "program App; begin end.\n");
    write_file(
        &root.join(".lint4d.toml"),
        "[rules.naming]\nconstant_style = \"PascalCase\"\n",
    );
    let malformed_fmt = app.join(".fmt4d.toml");
    write_file(&malformed_fmt, "[format\n");

    let mut server = TestServer::launch();
    server.initialize(root, json!({"projectFile": "app/App.dproj"}));
    server.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri(&main),
                "languageId": "pascal",
                "version": 1,
                "text": source
            }
        }),
    );
    let diagnostics = server.notification("textDocument/publishDiagnostics");
    assert!(
        diagnostics["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|diagnostic| diagnostic["code"] == "constant-naming")
    );

    let position = position_of(source, "badConst", 0);
    let end = Position::new(position.line, position.character + 8);
    let action_id = RequestId::from("project-sidecar-fallback-lint-action".to_string());
    server.send_request(
        action_id.clone(),
        "textDocument/codeAction",
        json!({
            "textDocument": {"uri": uri(&main)},
            "range": {"start": position, "end": end},
            "context": {
                "diagnostics": [{
                    "range": {"start": position, "end": end},
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
    assert!(action_response.error.is_none(), "{action_response:?}");
    assert_eq!(
        action_response.result.unwrap()[0]["title"],
        "Rename 'badConst' to 'BadConst'"
    );

    let format_id = RequestId::from("project-sidecar-malformed-fmt".to_string());
    server.send_request(
        format_id.clone(),
        "textDocument/formatting",
        json!({
            "textDocument": {"uri": uri(&main)},
            "options": {"tabSize": 2, "insertSpaces": true}
        }),
    );
    let format_response = server.response(&format_id);
    let format_error = format_response
        .error
        .expect("malformed fmt must refuse formatting");
    assert!(
        format_error
            .message
            .contains(&malformed_fmt.display().to_string())
    );
    server.shutdown();
}

#[test]
fn project_sidecar_malformed_lint_is_a_server_diagnostic_and_does_not_block_fmt() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let app = root.join("app");
    let main = app.join("Main.pas");
    let source =
        "unit Main;\ninterface\nimplementation\nprocedure Run;\nbegin\n  Log;\nend;\nend.\n";
    write_file(&main, source);
    write_file(
        &app.join("App.dproj"),
        "<Project><PropertyGroup><MainSource>Main.pas</MainSource></PropertyGroup></Project>",
    );
    write_file(&app.join("App.dpr"), "program App; begin end.\n");
    write_file(&app.join(".lint4d.toml"), "[rules\n");
    write_file(&root.join(".fmt4d.toml"), "[format]\nindent_size = 4\n");

    let mut server = TestServer::launch();
    server.initialize(root, json!({"projectFile": "app/App.dproj"}));
    server.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri(&main),
                "languageId": "pascal",
                "version": 1,
                "text": source
            }
        }),
    );
    let diagnostics = server.notification("textDocument/publishDiagnostics");
    let diagnostics = diagnostics["diagnostics"].as_array().unwrap();
    assert_eq!(diagnostics.len(), 1);
    assert_eq!(diagnostics[0]["code"], "pascal-lsp");
    assert!(
        diagnostics[0]["message"]
            .as_str()
            .unwrap()
            .contains(&app.join(".lint4d.toml").display().to_string())
    );

    let position = position_of(source, "Log", 0);
    let action_id = RequestId::from("project-sidecar-malformed-lint-action".to_string());
    server.send_request(
        action_id.clone(),
        "textDocument/codeAction",
        json!({
            "textDocument": {"uri": uri(&main)},
            "range": {"start": position, "end": position},
            "context": {
                "diagnostics": [{
                    "range": {"start": position, "end": position},
                    "severity": 2,
                    "code": "constant-naming",
                    "source": "lint4d",
                    "message": "naming violation"
                }],
                "only": ["quickfix"]
            }
        }),
    );
    let action_response = server.response(&action_id);
    let action_error = action_response
        .error
        .expect("malformed lint must refuse code actions");
    assert!(
        action_error
            .message
            .contains(&app.join(".lint4d.toml").display().to_string())
    );

    let format_id = RequestId::from("project-sidecar-malformed-lint-format".to_string());
    server.send_request(
        format_id.clone(),
        "textDocument/formatting",
        json!({
            "textDocument": {"uri": uri(&main)},
            "options": {"tabSize": 2, "insertSpaces": true}
        }),
    );
    let format_response = server.response(&format_id);
    assert!(format_response.error.is_none(), "{format_response:?}");
    let edits = format_response.result.unwrap();
    assert_eq!(edits.as_array().unwrap().len(), 1);
    assert!(edits[0]["newText"].as_str().unwrap().contains("\n    Log"));
    server.shutdown();
}

#[test]
fn project_sidecar_malformed_lint_replaces_previous_diagnostics() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let main = root.join("Main.pas");
    let config = root.join(".lint4d.toml");
    let source = "unit Main;\ninterface\nconst\n  badConst = 1;\nimplementation\nend.\n";
    write_file(&main, source);
    write_file(&config, "[rules.naming]\nconstant_style = \"UPPER_CASE\"\n");

    let mut server = TestServer::launch();
    server.initialize(root, Value::Null);
    server.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri(&main),
                "languageId": "pascal",
                "version": 1,
                "text": source
            }
        }),
    );
    let initial = server.notification("textDocument/publishDiagnostics");
    assert!(
        initial["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|diagnostic| diagnostic["code"] == "constant-naming")
    );

    write_file(&config, "[rules\n");
    server.send_notification(
        "workspace/didChangeWatchedFiles",
        json!({"changes": [{"uri": uri(&config), "type": 2}]}),
    );
    let replaced = server.notification("textDocument/publishDiagnostics");
    let diagnostics = replaced["diagnostics"].as_array().unwrap();
    assert_eq!(diagnostics.len(), 1);
    assert_eq!(diagnostics[0]["code"], "pascal-lsp");
    assert!(
        diagnostics[0]["message"]
            .as_str()
            .unwrap()
            .contains(&config.display().to_string())
    );

    write_file(&config, "[rules.naming]\nconstant_style = \"UPPER_CASE\"\n");
    server.send_notification(
        "workspace/didChangeWatchedFiles",
        json!({"changes": [{"uri": uri(&config), "type": 2}]}),
    );
    let recovered =
        server.notification_with_timeout("textDocument/publishDiagnostics", Duration::from_secs(2));
    assert!(
        recovered["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|diagnostic| diagnostic["code"] == "constant-naming")
    );
    server.shutdown();
}

#[test]
fn configuration_watch_refreshes_open_diagnostics() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let main = root.join("Main.pas");
    let config = root.join(".lint4d.toml");
    let source = "unit Main;\ninterface\nconst\n  badConst = 1;\nimplementation\nend.\n";
    write_file(&main, source);
    write_file(&config, "[rules.naming]\nconstant_style = \"UPPER_CASE\"\n");

    let mut server = TestServer::launch();
    server.initialize(root, Value::Null);
    server.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri(&main),
                "languageId": "pascal",
                "version": 1,
                "text": source
            }
        }),
    );
    let initial =
        server.notification_with_timeout("textDocument/publishDiagnostics", Duration::from_secs(2));
    assert!(
        initial["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|diagnostic| diagnostic["code"] == "constant-naming")
    );

    write_file(&config, "[rules]\nconstant-naming = \"off\"\n");
    server.send_notification(
        "workspace/didChangeWatchedFiles",
        json!({"changes": [{"uri": uri(&config), "type": 2}]}),
    );
    let published =
        server.notification_with_timeout("textDocument/publishDiagnostics", Duration::from_secs(2));
    assert!(
        !published["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|diagnostic| diagnostic["code"] == "constant-naming")
    );
    server.shutdown();
}

#[test]
fn configuration_watch_tracks_absent_higher_priority_fallback_candidates() {
    let temp = tempfile::tempdir().unwrap();
    let repository = temp.path().join("repository");
    let workspace_root = repository.join("nested-workspace");
    let main = workspace_root.join("Main.pas");
    let repository_config = repository.join(".lint4d.toml");
    let workspace_config = workspace_root.join(".lint4d.toml");
    let source = "unit Main;\ninterface\nconst\n  badConst = 1;\nimplementation\nend.\n";
    fs::create_dir_all(&workspace_root).unwrap();
    write_file(&repository.join(".git"), "gitdir: /outside/worktree\n");
    write_file(&main, source);
    write_file(
        &repository_config,
        "[rules.naming]\nconstant_style = \"UPPER_CASE\"\n",
    );

    let mut server = TestServer::launch();
    server.initialize(&workspace_root, Value::Null);
    server.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri(&main),
                "languageId": "pascal",
                "version": 1,
                "text": source
            }
        }),
    );
    let initial =
        server.notification_with_timeout("textDocument/publishDiagnostics", Duration::from_secs(2));
    assert!(
        initial["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|diagnostic| diagnostic["code"] == "constant-naming")
    );

    write_file(&workspace_config, "[rules]\nconstant-naming = \"off\"\n");
    server.send_notification(
        "workspace/didChangeWatchedFiles",
        json!({"changes": [{"uri": uri(&workspace_config), "type": 1}]}),
    );
    let disabled =
        server.notification_with_timeout("textDocument/publishDiagnostics", Duration::from_secs(2));
    assert!(
        !disabled["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|diagnostic| diagnostic["code"] == "constant-naming")
    );

    fs::remove_file(&workspace_config).unwrap();
    server.send_notification(
        "workspace/didChangeWatchedFiles",
        json!({"changes": [{"uri": uri(&workspace_config), "type": 3}]}),
    );
    let restored =
        server.notification_with_timeout("textDocument/publishDiagnostics", Duration::from_secs(2));
    assert!(
        restored["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|diagnostic| diagnostic["code"] == "constant-naming")
    );
    server.shutdown();
}

#[test]
fn configuration_watch_keeps_unrelated_project_scope_diagnostics() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let first = root.join("first");
    let second = root.join("second");
    let first_main = first.join("Main.pas");
    let second_main = second.join("Main.pas");
    let first_config = first.join(".lint4d.toml");
    let second_config = second.join(".lint4d.toml");
    let source = "unit Main;\ninterface\nconst\n  badConst = 1;\nimplementation\nend.\n";

    for (directory, project_name, main) in [
        (&first, "First", &first_main),
        (&second, "Second", &second_main),
    ] {
        write_file(main, source);
        write_file(
            &directory.join(format!("{project_name}.dproj")),
            "<Project><PropertyGroup><MainSource>Main.pas</MainSource></PropertyGroup></Project>",
        );
        write_file(
            &directory.join(format!("{project_name}.dpr")),
            &format!("program {project_name}; begin end.\n"),
        );
    }
    write_file(
        &first_config,
        "[rules.naming]\nconstant_style = \"UPPER_CASE\"\n",
    );
    write_file(
        &second_config,
        "[rules.naming]\nconstant_style = \"UPPER_CASE\"\n",
    );

    let mut server = TestServer::launch();
    server.initialize(root, Value::Null);
    for main in [&first_main, &second_main] {
        server.send_notification(
            "textDocument/didOpen",
            json!({
                "textDocument": {
                    "uri": uri(main),
                    "languageId": "pascal",
                    "version": 1,
                    "text": source
                }
            }),
        );
    }
    for main in [&first_main, &second_main] {
        let diagnostics = diagnostics_for_uri(&mut server, &uri(main));
        assert!(
            diagnostics["diagnostics"]
                .as_array()
                .unwrap()
                .iter()
                .any(|diagnostic| diagnostic["code"] == "constant-naming"),
            "each project scope should initially report its own naming diagnostic: {diagnostics}"
        );
    }

    write_file(&first_config, "[rules]\nconstant-naming = \"off\"\n");
    server.send_notification(
        "workspace/didChangeWatchedFiles",
        json!({"changes": [{"uri": uri(&first_config), "type": 2}]}),
    );
    let first_after = diagnostics_for_uri(&mut server, &uri(&first_main));
    assert!(
        !first_after["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|diagnostic| diagnostic["code"] == "constant-naming")
    );
    let second_after = diagnostics_for_uri(&mut server, &uri(&second_main));
    assert!(
        second_after["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|diagnostic| diagnostic["code"] == "constant-naming"),
        "republishing the unrelated scope must preserve its effective settings: {second_after}"
    );
    server.shutdown();
}

#[test]
fn project_sidecar_lint_excludes_do_not_hide_rename_consumers() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let target = root.join("Target.pas");
    let consumer = root.join("excluded/Consumer.pas");
    let target_source = "unit Target;\ninterface\nconst\n  BadConst = 1;\nimplementation\nend.\n";
    let consumer_source = "unit Consumer;\ninterface\nuses Target;\nimplementation\nprocedure Run;\nbegin\n  UseValue(BadConst);\nend;\nend.\n";
    write_file(&target, target_source);
    write_file(&consumer, consumer_source);
    write_file(
        &root.join(".lint4d.toml"),
        "[lint4d]\nexclude = [\"excluded/**\"]\n",
    );

    let mut server = TestServer::launch();
    server.initialize(root, Value::Null);
    let position = position_of(target_source, "BadConst", 0);
    let request_id = RequestId::from("project-sidecar-excluded-consumer-rename".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/rename",
        json!({
            "textDocument": {"uri": uri(&target)},
            "position": position,
            "newName": "GoodConst"
        }),
    );
    let response = server.response(&request_id);
    assert!(response.error.is_none(), "{response:?}");
    let edit = response.result.unwrap();
    let edited_uris = workspace_edit_uris(&edit);
    assert!(edited_uris.contains(&uri(&target).to_string()));
    assert!(
        edited_uris.contains(&uri(&consumer).to_string()),
        "lint exclusion must not hide a rename consumer: {edit}"
    );
    server.shutdown();
}

#[test]
fn project_sidecar_exclusion_transition_clears_diagnostics_and_preserves_rename_scope() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let target = root.join("Target.pas");
    let consumer = root.join("excluded/Consumer.pas");
    let config = root.join(".lint4d.toml");
    let target_source = "unit Target;\ninterface\nconst\n  BadConst = 1;\nimplementation\nend.\n";
    let consumer_source = "unit Consumer;\ninterface\nuses Target;\nimplementation\nprocedure Run;\nbegin\n  UseValue(BadConst);\nend;\nend.\n";
    write_file(&target, target_source);
    write_file(&consumer, consumer_source);
    write_file(&config, "[rules.naming]\nconstant_style = \"UPPER_CASE\"\n");

    let mut server = TestServer::launch();
    server.initialize_with_action_support(root, Value::Null);
    server.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri(&target),
                "languageId": "pascal",
                "version": 1,
                "text": target_source
            }
        }),
    );
    let diagnostics = server.notification("textDocument/publishDiagnostics");
    assert!(!diagnostics["diagnostics"].as_array().unwrap().is_empty());

    let position = position_of(target_source, "BadConst", 0);
    let action_id = RequestId::from("action-before-lint-exclusion".to_string());
    server.send_request(
        action_id.clone(),
        "textDocument/codeAction",
        json!({
            "textDocument": {"uri": uri(&target)},
            "range": {"start": position, "end": Position::new(position.line, position.character + 8)},
            "context": {
                "diagnostics": [{
                    "range": {"start": position, "end": Position::new(position.line, position.character + 8)},
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
    assert!(action_response.error.is_none(), "{action_response:?}");
    let action = action_response.result.unwrap()[0].clone();
    assert!(action["edit"].is_null());

    let before_id = RequestId::from("rename-before-lint-exclusion".to_string());
    server.send_request(
        before_id.clone(),
        "textDocument/rename",
        json!({
            "textDocument": {"uri": uri(&target)},
            "position": position_of(target_source, "BadConst", 0),
            "newName": "GoodConst"
        }),
    );
    let before = server.response(&before_id);
    assert!(before.error.is_none(), "{before:?}");
    assert!(workspace_edit_uris(&before.result.unwrap()).contains(&uri(&consumer).to_string()));

    write_file(
        &config,
        "[lint4d]\nexclude = [\"Target.pas\", \"excluded/**\"]\n[rules.naming]\nconstant_style = \"UPPER_CASE\"\n",
    );
    let resolve_id = RequestId::from("resolve-after-lint-exclusion".to_string());
    server.send_request(resolve_id.clone(), "codeAction/resolve", action);
    let resolve_response = server.response(&resolve_id);
    let resolve_error = resolve_response
        .error
        .expect("a newly excluded action must not resolve");
    assert!(
        resolve_error
            .message
            .contains("excluded by lint configuration")
    );

    server.send_notification(
        "workspace/didChangeWatchedFiles",
        json!({"changes": [{"uri": uri(&config), "type": 2}]}),
    );
    let diagnostics = server.notification("textDocument/publishDiagnostics");
    assert!(
        diagnostics["diagnostics"].as_array().unwrap().is_empty(),
        "adding a lint exclusion must replace prior diagnostics: {diagnostics}"
    );

    let after_id = RequestId::from("rename-after-lint-exclusion".to_string());
    server.send_request(
        after_id.clone(),
        "textDocument/rename",
        json!({
            "textDocument": {"uri": uri(&target)},
            "position": position_of(target_source, "BadConst", 0),
            "newName": "GoodConst"
        }),
    );
    let after = server.response(&after_id);
    assert!(after.error.is_none(), "{after:?}");
    assert!(
        workspace_edit_uris(&after.result.unwrap()).contains(&uri(&consumer).to_string()),
        "new lint exclusions must not hide rename consumers"
    );
    server.shutdown();
}

#[test]
fn project_context_keeps_malformed_configuration_as_a_warning() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let main = root.join("Main.pas");
    let config = root.join(".lint4d.toml");
    write_file(&main, "unit Main; interface implementation end.");
    write_file(
        &root.join("Main.dproj"),
        "<Project><PropertyGroup><MainSource>Main.pas</MainSource></PropertyGroup></Project>",
    );
    write_file(&config, "[rules\n");

    let mut server = TestServer::launch();
    server.initialize(root, Value::Null);
    let id = RequestId::from("malformed-project-config".to_string());
    server.send_request(
        id.clone(),
        "pascal/projectContext",
        json!({"textDocument": {"uri": uri(&main)}}),
    );
    let response = server.response(&id);
    assert!(response.error.is_none(), "{response:?}");
    let context = response.result.unwrap();
    assert!(context["lintConfigUri"].is_null());
    assert!(
        context["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|warning| warning.as_str().unwrap().contains(".lint4d.toml"))
    );
    let format_id = RequestId::from("malformed-project-format".to_string());
    server.send_request(
        format_id.clone(),
        "textDocument/formatting",
        json!({"textDocument": {"uri": uri(&main)}, "options": {}}),
    );
    let format_response = server.response(&format_id);
    assert!(format_response.error.is_some(), "{format_response:?}");
    server.shutdown();
}

#[test]
fn switching_projects_preserves_an_unsaved_overlay() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let main = root.join("Main.pas");
    let on_disk = "unit Main; interface implementation end.\n";
    let overlay = "unit Main;\ninterface\nimplementation\nprocedure Run;\nbegin\n  S := 1; with Obj do begin end;\nend;\nend.\n";
    write_file(&main, on_disk);
    for name in ["A", "B"] {
        write_file(
            &root.join(format!("{name}.dproj")),
            "<Project><PropertyGroup><MainSource>Main.pas</MainSource></PropertyGroup></Project>",
        );
    }

    let mut server = TestServer::launch();
    server.initialize(root, Value::Null);
    server.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri(&main),
                "languageId": "pascal",
                "version": 7,
                "text": overlay
            }
        }),
    );
    let _ = server.notification("textDocument/publishDiagnostics");

    let select_id = RequestId::from("overlay-project-select".to_string());
    server.send_request(
        select_id.clone(),
        "pascal/selectProject",
        json!({
            "textDocument": {"uri": uri(&main)},
            "projectUri": uri(&root.join("B.dproj"))
        }),
    );
    assert!(server.response(&select_id).error.is_none());
    let diagnostics = server.notification("textDocument/publishDiagnostics");
    assert_eq!(diagnostics["version"], 7);
    assert!(
        diagnostics["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|diagnostic| diagnostic["code"] == "with-statement")
    );
    server.shutdown();
}

#[test]
fn project_context_reports_singleton_and_proven_owner_modes() {
    let temp = tempfile::tempdir().unwrap();
    let singleton_root = temp.path().join("singleton");
    let singleton_main = singleton_root.join("Main.pas");
    write_file(&singleton_main, "unit Main; interface implementation end.");
    let singleton_project = singleton_root.join("Only.dproj");
    write_file(
        &singleton_project,
        "<Project><PropertyGroup><MainSource>Main.pas</MainSource></PropertyGroup></Project>",
    );

    let owner_root = temp.path().join("owner");
    let owner_source = owner_root.join("Owned.pas");
    write_file(&owner_source, "unit Owned; interface implementation end.");
    write_file(
        &owner_root.join("OwnerA.dpr"),
        "program OwnerA; uses Owned in 'Owned.pas'; begin end.",
    );
    write_file(&owner_root.join("OwnerB.dpr"), "program OwnerB; begin end.");
    for name in ["OwnerA", "OwnerB"] {
        write_file(
            &owner_root.join(format!("{name}.dproj")),
            &format!(
                "<Project><PropertyGroup><MainSource>{name}.dpr</MainSource></PropertyGroup></Project>"
            ),
        );
    }

    let mut server = TestServer::launch();
    server.initialize(temp.path(), Value::Null);
    for (id, document, mode, selected) in [
        (
            "singleton-context",
            &singleton_main,
            "automatic",
            Some(singleton_project),
        ),
        (
            "owner-context",
            &owner_source,
            "automatic",
            Some(owner_root.join("OwnerA.dproj")),
        ),
    ] {
        let id = RequestId::from(id.to_string());
        server.send_request(
            id.clone(),
            "pascal/projectContext",
            json!({"textDocument": {"uri": uri(document)}}),
        );
        let response = server.response(&id);
        assert!(response.error.is_none(), "{response:?}");
        let context = response.result.unwrap();
        assert_eq!(context["selectionMode"], mode);
        let selected = selected.expect("expected selected project");
        assert_eq!(context["selectedProjectUri"], uri(&selected).to_string());
    }
    server.shutdown();
}

#[test]
fn selecting_a_project_switches_same_named_unit_bindings_in_its_scope() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let main = root.join("Main.pas");
    let a_unit = root.join("a/Shared.pas");
    let b_unit = root.join("b/Shared.pas");
    let main_source = "unit Main;\ninterface\nuses Shared;\nimplementation\nprocedure Run;\nbegin\n  Routine;\nend;\nend.\n";
    let unit_source = "unit Shared;\ninterface\nprocedure Routine;\nimplementation\nprocedure Routine; begin end;\nend.\n";
    write_file(&main, main_source);
    write_file(&a_unit, unit_source);
    write_file(&b_unit, unit_source);
    for (name, path) in [("A", "a/Shared.pas"), ("B", "b/Shared.pas")] {
        write_file(
            &root.join(format!("{name}.dproj")),
            &format!(
                "<Project><PropertyGroup><MainSource>Main.pas</MainSource></PropertyGroup><ItemGroup><DCCReference Include=\"{path}\" /></ItemGroup></Project>"
            ),
        );
    }

    let mut server = TestServer::launch();
    server.initialize(root, Value::Null);
    for (id, project, expected) in [
        ("select-a", root.join("A.dproj"), a_unit),
        ("select-b", root.join("B.dproj"), b_unit),
    ] {
        let select_id = RequestId::from(id.to_string());
        server.send_request(
            select_id.clone(),
            "pascal/selectProject",
            json!({
                "textDocument": {"uri": uri(&main)},
                "projectUri": uri(&project)
            }),
        );
        assert!(server.response(&select_id).error.is_none());
        let navigation_id = RequestId::from(format!("{id}-navigation"));
        server.send_request(
            navigation_id.clone(),
            "textDocument/declaration",
            navigation_params(&main, main_source, "Routine", 0),
        );
        let locations = result_locations(server.response(&navigation_id));
        assert_eq!(locations.len(), 1, "{id} should resolve one declaration");
        assert_eq!(locations[0]["uri"], uri(&expected).to_string());
    }
    server.shutdown();
}

#[test]
fn naming_code_actions_use_the_selected_project_configuration() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let project = root.join("app");
    let main = project.join("Main.pas");
    let source = "unit Main;\ninterface\nconst\n  badConst = 1;\nimplementation\nend.\n";
    write_file(&main, source);
    write_file(
        &project.join("App.dproj"),
        "<Project><PropertyGroup><MainSource>Main.pas</MainSource></PropertyGroup></Project>",
    );
    write_file(
        &root.join(".lint4d.toml"),
        "[rules.naming]\nconstant_style = \"PascalCase\"\n",
    );
    write_file(
        &project.join(".lint4d.toml"),
        "[rules.naming]\nconstant_style = \"PascalCase\"\n",
    );
    let start = position_of(source, "badConst", 0);
    let end = Position::new(start.line, start.character + 8);

    let mut server = TestServer::launch();
    server.initialize(root, Value::Null);
    let id = RequestId::from("project-code-action".to_string());
    server.send_request(
        id.clone(),
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
    let response = server.response(&id);
    assert!(response.error.is_none(), "{response:?}");
    assert_eq!(
        response.result.unwrap()[0]["title"],
        "Rename 'badConst' to 'BadConst'"
    );
    server.shutdown();
}

#[test]
fn standalone_naming_actions_match_workspace_configuration_precedence() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("workspace");
    let main = root.join("src/Main.pas");
    let source = "unit Main;\ninterface\nconst\n  BadConst = 1;\nimplementation\nend.\n";
    write_file(&main, source);
    write_file(
        &root.join(".lint4d.toml"),
        "[rules.naming]\nconstant_style = \"UPPER_CASE\"\n",
    );
    write_file(
        &root.join("src/.lint4d.toml"),
        "[rules.naming]\nconstant_style = \"PascalCase\"\n",
    );

    let start = position_of(source, "BadConst", 0);
    let end = Position::new(start.line, start.character + 8);
    let mut server = TestServer::launch();
    server.initialize(&root, Value::Null);
    server.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri(&main),
                "languageId": "pascal",
                "version": 1,
                "text": source
            }
        }),
    );
    let diagnostics = server.notification("textDocument/publishDiagnostics");
    assert!(
        diagnostics["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|diagnostic| diagnostic["code"] == "constant-naming"),
        "diagnostics must use the workspace-level standalone configuration: {diagnostics}"
    );

    let action_id = RequestId::from("standalone-config-action".to_string());
    server.send_request(
        action_id.clone(),
        "textDocument/codeAction",
        json!({
            "textDocument": {"uri": uri(&main)},
            "range": {"start": start, "end": end},
            "context": {
                "diagnostics": [{
                    "range": {"start": start, "end": end},
                    "severity": 2,
                    "code": "constant-naming",
                    "source": "lint4d",
                    "message": "naming violation"
                }],
                "only": ["quickfix"]
            }
        }),
    );
    let response = server.response(&action_id);
    assert!(response.error.is_none(), "{response:?}");
    assert_eq!(
        response.result.unwrap()[0]["title"],
        "Rename 'BadConst' to 'BAD_CONST'"
    );
    server.shutdown();
}

#[test]
fn rescheduled_diagnostics_follow_directory_override_then_configured_fallback_after_reset() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let first = root.join("Main.pas");
    let second = root.join("other/Other.pas");
    let chosen_project = root.join("app/Chosen.dproj");
    let local_project = root.join("Local.dproj");
    let source = "unit Main;\ninterface\nconst\n  BadConst = 1;\nimplementation\nend.\n";
    let second_source = source.replace("unit Main", "unit Other");
    write_file(&first, source);
    write_file(&second, &second_source);
    write_file(
        &chosen_project,
        "<Project><PropertyGroup><MainSource>Chosen.pas</MainSource></PropertyGroup></Project>",
    );
    write_file(
        &local_project,
        "<Project><PropertyGroup><MainSource>Main.pas</MainSource></PropertyGroup></Project>",
    );
    write_file(
        &root.join(".lint4d.toml"),
        "[rules.naming]\nconstant_style = \"PascalCase\"\n",
    );
    write_file(
        &root.join("app/.lint4d.toml"),
        "[rules.naming]\nconstant_style = \"UPPER_CASE\"\n",
    );

    let mut server = TestServer::launch();
    server.initialize(root, json!({"projectFile": "app/Chosen.dproj"}));
    for (path, text) in [(&first, source), (&second, second_source.as_str())] {
        server.send_notification(
            "textDocument/didOpen",
            json!({
                "textDocument": {
                    "uri": uri(path),
                    "languageId": "pascal",
                    "version": 1,
                    "text": text
                }
            }),
        );
    }
    let first_initial = diagnostics_for_uri(&mut server, &uri(&first));
    let second_initial = diagnostics_for_uri(&mut server, &uri(&second));
    for diagnostics in [&first_initial, &second_initial] {
        assert!(
            diagnostics["diagnostics"]
                .as_array()
                .unwrap()
                .iter()
                .any(|diagnostic| diagnostic["code"] == "constant-naming")
        );
    }

    let select_id = RequestId::from("diagnostics-local-select".to_string());
    server.send_request(
        select_id.clone(),
        "pascal/selectProject",
        json!({
            "textDocument": {"uri": uri(&first)},
            "projectUri": uri(&local_project)
        }),
    );
    assert!(server.response(&select_id).error.is_none());
    let _ = diagnostics_for_uri(&mut server, &uri(&first));
    let second_after_select = diagnostics_for_uri(&mut server, &uri(&second));
    assert!(
        !second_after_select["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|diagnostic| diagnostic["code"] == "constant-naming")
    );

    let context_id = RequestId::from("diagnostics-local-context".to_string());
    server.send_request(
        context_id.clone(),
        "pascal/projectContext",
        json!({"textDocument": {"uri": uri(&second)}}),
    );
    let context_response = server.response(&context_id);
    assert!(context_response.error.is_none(), "{context_response:?}");
    assert_eq!(
        context_response.result.unwrap()["selectedProjectUri"],
        uri(&local_project).to_string()
    );

    let navigation_id = RequestId::from("diagnostics-local-navigation".to_string());
    server.send_request(
        navigation_id.clone(),
        "textDocument/declaration",
        navigation_params(&second, &second_source, "BadConst", 0),
    );
    let _ = result_locations(server.response(&navigation_id));

    let reset_id = RequestId::from("diagnostics-local-reset".to_string());
    server.send_request(
        reset_id.clone(),
        "pascal/selectProject",
        json!({
            "textDocument": {"uri": uri(&first)},
            "projectUri": null
        }),
    );
    assert!(server.response(&reset_id).error.is_none());
    let _ = diagnostics_for_uri(&mut server, &uri(&first));
    let second_after_reset = diagnostics_for_uri(&mut server, &uri(&second));
    assert!(
        second_after_reset["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|diagnostic| diagnostic["code"] == "constant-naming")
    );
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
fn open_shared_dependency_retains_its_owner_after_a_peer_project_switch() {
    let temp = tempfile::tempdir().unwrap();
    let fixture = open_shared_owner_fixture(temp.path());
    let root = temp.path();
    let mut server = TestServer::launch();
    server.initialize(root, Value::Null);

    let select_a = RequestId::from("open-owner-select-a".to_string());
    server.send_request(
        select_a.clone(),
        "pascal/selectProject",
        json!({
            "textDocument": {"uri": uri(&fixture.a_main)},
            "projectUri": uri(&fixture.a_project)
        }),
    );
    assert!(server.response(&select_a).error.is_none());

    server.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri(&fixture.shared),
                "languageId": "pascal",
                "version": 1,
                "text": &fixture.shared_source
            }
        }),
    );
    let _ = server.notification("textDocument/publishDiagnostics");

    let select_b = RequestId::from("open-owner-select-b".to_string());
    server.send_request(
        select_b.clone(),
        "pascal/selectProject",
        json!({
            "textDocument": {"uri": uri(&fixture.b_main)},
            "projectUri": uri(&fixture.b_project)
        }),
    );
    assert!(server.response(&select_b).error.is_none());

    let b_navigation = RequestId::from("open-owner-b-navigation".to_string());
    server.send_request(
        b_navigation.clone(),
        "textDocument/declaration",
        navigation_params(&fixture.b_main, &fixture.main_source, "Run", 0),
    );
    assert_eq!(
        result_locations(server.response(&b_navigation))[0]["uri"],
        uri(&fixture.shared).to_string()
    );

    let shared_navigation = RequestId::from("open-owner-shared-navigation".to_string());
    server.send_request(
        shared_navigation.clone(),
        "textDocument/declaration",
        navigation_params(&fixture.shared, &fixture.shared_source, "ConfigRoutine", 0),
    );
    assert_eq!(
        result_locations(server.response(&shared_navigation))[0]["uri"],
        uri(&fixture.a_config).to_string(),
        "an open shared unit must retain A's owner after B clears disposable contexts"
    );
    server.shutdown();
}

#[test]
fn open_shared_dependency_does_not_adopt_a_peer_after_its_owner_is_removed() {
    let temp = tempfile::tempdir().unwrap();
    let fixture = open_shared_owner_fixture(temp.path());
    let root = temp.path();
    let mut server = TestServer::launch();
    server.initialize(root, Value::Null);

    let select_a = RequestId::from("open-invalid-owner-select-a".to_string());
    server.send_request(
        select_a.clone(),
        "pascal/selectProject",
        json!({
            "textDocument": {"uri": uri(&fixture.a_main)},
            "projectUri": uri(&fixture.a_project)
        }),
    );
    assert!(server.response(&select_a).error.is_none());

    server.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri(&fixture.shared),
                "languageId": "pascal",
                "version": 1,
                "text": &fixture.shared_source
            }
        }),
    );
    let _ = server.notification("textDocument/publishDiagnostics");

    fs::remove_file(&fixture.a_project).unwrap();
    server.send_notification(
        "workspace/didChangeWatchedFiles",
        json!({"changes": [{"uri": uri(&fixture.a_project), "type": 3}]}),
    );

    let select_b = RequestId::from("open-invalid-owner-select-b".to_string());
    server.send_request(
        select_b.clone(),
        "pascal/selectProject",
        json!({
            "textDocument": {"uri": uri(&fixture.b_main)},
            "projectUri": uri(&fixture.b_project)
        }),
    );
    assert!(server.response(&select_b).error.is_none());

    let b_navigation = RequestId::from("open-invalid-owner-b-navigation".to_string());
    server.send_request(
        b_navigation.clone(),
        "textDocument/declaration",
        navigation_params(&fixture.b_main, &fixture.main_source, "Run", 0),
    );
    assert_eq!(
        result_locations(server.response(&b_navigation))[0]["uri"],
        uri(&fixture.shared).to_string()
    );

    let shared_navigation = RequestId::from("open-invalid-owner-shared-navigation".to_string());
    server.send_request(
        shared_navigation.clone(),
        "textDocument/declaration",
        navigation_params(&fixture.shared, &fixture.shared_source, "ConfigRoutine", 0),
    );
    assert!(
        result_locations(server.response(&shared_navigation)).is_empty(),
        "a removed open owner must not be replaced by B's importing context"
    );
    server.shutdown();
}

#[test]
fn directory_override_survives_peer_dependency_load_before_direct_navigation() {
    let temp = tempfile::tempdir().unwrap();
    let fixture = open_shared_owner_fixture(temp.path());
    let root = temp.path();
    let mut server = TestServer::launch();
    server.initialize(root, Value::Null);

    server.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri(&fixture.shared),
                "languageId": "pascal",
                "version": 1,
                "text": &fixture.shared_source
            }
        }),
    );
    let _ = server.notification("textDocument/publishDiagnostics");

    let select_a = RequestId::from("override-owner-select-a".to_string());
    server.send_request(
        select_a.clone(),
        "pascal/selectProject",
        json!({
            "textDocument": {"uri": uri(&fixture.a_main)},
            "projectUri": uri(&fixture.a_project)
        }),
    );
    assert!(server.response(&select_a).error.is_none());

    let b_navigation = RequestId::from("override-owner-b-navigation".to_string());
    server.send_request(
        b_navigation.clone(),
        "textDocument/declaration",
        navigation_params(&fixture.b_main, &fixture.main_source, "Run", 0),
    );
    assert_eq!(
        result_locations(server.response(&b_navigation))[0]["uri"],
        uri(&fixture.shared).to_string()
    );

    let context_id = RequestId::from("override-owner-shared-context".to_string());
    server.send_request(
        context_id.clone(),
        "pascal/projectContext",
        json!({"textDocument": {"uri": uri(&fixture.shared)}}),
    );
    let context = server.response(&context_id);
    assert!(context.error.is_none(), "{context:?}");
    assert_eq!(
        context.result.unwrap()["selectedProjectUri"],
        uri(&fixture.a_project).to_string(),
        "the cached shared context must follow the current A directory override"
    );

    let shared_navigation = RequestId::from("override-owner-shared-navigation".to_string());
    server.send_request(
        shared_navigation.clone(),
        "textDocument/declaration",
        navigation_params(&fixture.shared, &fixture.shared_source, "ConfigRoutine", 0),
    );
    assert_eq!(
        result_locations(server.response(&shared_navigation))[0]["uri"],
        uri(&fixture.a_config).to_string(),
        "peer dependency loading must not replace the overridden shared context"
    );
    server.shutdown();
}

#[test]
fn inherited_closed_dependency_follows_new_runtime_project_selection() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path().join("workspace");
    let main = root.join("Main.pas");
    let shared = root.join("Shared.pas");
    let project_a = root.join("A.dproj");
    let project_b = root.join("B.dproj");
    let provider_a = root.join("A/Provider.pas");
    let provider_b = root.join("B/Provider.pas");
    let main_source = "unit Main;\ninterface\nuses Shared;\nimplementation\nprocedure Use;\nbegin\n  Run;\nend;\nend.\n";
    let shared_source = "unit Shared;\ninterface\nuses Provider;\nprocedure Run;\nimplementation\nprocedure Run;\nbegin\n  ProviderRoutine;\nend;\nend.\n";
    let project = |provider: &str| {
        format!(
            "<Project><PropertyGroup><MainSource>Main.pas</MainSource></PropertyGroup><ItemGroup><DCCReference Include=\"Shared.pas\" /><DCCReference Include=\"{provider}\" /></ItemGroup></Project>"
        )
    };

    write_file(&main, main_source);
    write_file(&shared, shared_source);
    write_file(&project_a, &project("A/Provider.pas"));
    write_file(&project_b, &project("B/Provider.pas"));
    write_file(
        &provider_a,
        "unit Provider;\ninterface\nprocedure ProviderRoutine;\nimplementation\nprocedure ProviderRoutine; begin end;\nend.\n",
    );
    write_file(
        &provider_b,
        "unit Provider;\ninterface\nprocedure ProviderRoutine;\nimplementation\nprocedure ProviderRoutine; begin end;\nend.\n",
    );

    let mut server = TestServer::launch();
    server.initialize(&root, json!({"projectFile": "A.dproj"}));

    let load_id = RequestId::from("inherited-runtime-override-load".to_string());
    server.send_request(
        load_id.clone(),
        "textDocument/declaration",
        navigation_params(&main, main_source, "Run", 0),
    );
    assert_eq!(
        result_locations(server.response(&load_id))[0]["uri"],
        uri(&shared).to_string()
    );

    let select_id = RequestId::from("inherited-runtime-override-select-b".to_string());
    server.send_request(
        select_id.clone(),
        "pascal/selectProject",
        json!({
            "textDocument": {"uri": uri(&main)},
            "projectUri": uri(&project_b)
        }),
    );
    assert!(server.response(&select_id).error.is_none());

    let provider_id = RequestId::from("inherited-runtime-override-provider".to_string());
    server.send_request(
        provider_id.clone(),
        "textDocument/declaration",
        navigation_params(&shared, shared_source, "ProviderRoutine", 0),
    );
    assert_eq!(
        result_locations(server.response(&provider_id))[0]["uri"],
        uri(&provider_b).to_string(),
        "an inherited closed dependency must follow the newly selected runtime project"
    );
    server.shutdown();
}

#[test]
fn project_context_retains_shared_owner_selection_outside_source_ancestry() {
    let temp = tempfile::tempdir().unwrap();
    let fixture = shared_owner_fixture(temp.path());
    let root = temp.path();
    let mut server = TestServer::launch();
    server.initialize(root, Value::Null);

    let select_a = RequestId::from("shared-context-select-a".to_string());
    server.send_request(
        select_a.clone(),
        "pascal/selectProject",
        json!({
            "textDocument": {"uri": uri(&fixture.a_main)},
            "projectUri": uri(&fixture.a_project)
        }),
    );
    assert!(server.response(&select_a).error.is_none());

    let load_shared = RequestId::from("shared-context-load".to_string());
    server.send_request(
        load_shared.clone(),
        "textDocument/declaration",
        navigation_params(&fixture.a_main, &fixture.main_source, "Run", 0),
    );
    assert_eq!(
        result_locations(server.response(&load_shared))[0]["uri"],
        uri(&fixture.shared).to_string()
    );

    let before_id = RequestId::from("shared-context-before-removal".to_string());
    server.send_request(
        before_id.clone(),
        "pascal/projectContext",
        json!({"textDocument": {"uri": uri(&fixture.shared)}}),
    );
    let before = server.response(&before_id);
    assert!(before.error.is_none(), "{before:?}");
    let before = before.result.unwrap();
    assert_eq!(before["selectionMode"], "directory");
    assert_eq!(
        before["selectedProjectUri"],
        uri(&fixture.a_project).to_string()
    );

    fs::remove_file(&fixture.a_project).unwrap();
    server.send_notification(
        "workspace/didChangeWatchedFiles",
        json!({"changes": [{"uri": uri(&fixture.a_project), "type": 3}]}),
    );

    let after_id = RequestId::from("shared-context-after-removal".to_string());
    server.send_request(
        after_id.clone(),
        "pascal/projectContext",
        json!({"textDocument": {"uri": uri(&fixture.shared)}}),
    );
    let after = server.response(&after_id);
    assert!(after.error.is_none(), "{after:?}");
    let after = after.result.unwrap();
    assert_eq!(after["selectionMode"], "invalid");
    assert_eq!(
        after["scopeUri"],
        uri(&root.join("A")).to_string(),
        "an invalid retained owner must keep its selection scope"
    );
    assert!(
        after["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|warning| warning
                .as_str()
                .unwrap_or_default()
                .contains(&fixture.a_project.display().to_string())),
        "invalid retained owner warning must be preserved: {after}"
    );
    server.shutdown();
}

#[test]
fn automatic_clears_an_advertised_inherited_owner_without_clearing_peer_selection() {
    let temp = tempfile::tempdir().unwrap();
    let fixture = shared_owner_fixture(temp.path());
    let root = temp.path();
    let mut server = TestServer::launch();
    server.initialize(root, Value::Null);

    let select_a = RequestId::from("automatic-inherited-select-a".to_string());
    server.send_request(
        select_a.clone(),
        "pascal/selectProject",
        json!({
            "textDocument": {"uri": uri(&fixture.a_main)},
            "projectUri": uri(&fixture.a_project)
        }),
    );
    assert!(server.response(&select_a).error.is_none());

    let load_shared = RequestId::from("automatic-inherited-load-shared".to_string());
    server.send_request(
        load_shared.clone(),
        "textDocument/declaration",
        navigation_params(&fixture.a_main, &fixture.main_source, "Run", 0),
    );
    assert_eq!(
        result_locations(server.response(&load_shared))[0]["uri"],
        uri(&fixture.shared).to_string()
    );

    let select_b = RequestId::from("automatic-inherited-select-b".to_string());
    server.send_request(
        select_b.clone(),
        "pascal/selectProject",
        json!({
            "textDocument": {"uri": uri(&fixture.b_main)},
            "projectUri": uri(&fixture.b_project)
        }),
    );
    assert!(server.response(&select_b).error.is_none());

    let automatic = RequestId::from("automatic-inherited-reset".to_string());
    server.send_request(
        automatic.clone(),
        "pascal/selectProject",
        json!({
            "textDocument": {"uri": uri(&fixture.shared)},
            "projectUri": null
        }),
    );
    let automatic_response = server.response(&automatic);
    assert!(
        automatic_response.error.is_none(),
        "Automatic reset failed: {automatic_response:?}"
    );
    let automatic_context = automatic_response.result.as_ref().unwrap();
    assert_eq!(automatic_context["selectionMode"], "standalone");
    assert!(
        automatic_context["selectedProjectUri"].is_null(),
        "Automatic must clear the retained inherited project: {automatic_context}"
    );

    let peer_context_id = RequestId::from("automatic-inherited-peer-context".to_string());
    server.send_request(
        peer_context_id.clone(),
        "pascal/projectContext",
        json!({"textDocument": {"uri": uri(&fixture.b_main)}}),
    );
    let peer_context = server.response(&peer_context_id);
    assert!(
        peer_context.error.is_none(),
        "peer context failed: {peer_context:?}"
    );
    assert_eq!(
        peer_context.result.as_ref().unwrap()["selectedProjectUri"],
        uri(&fixture.b_project).to_string(),
        "Automatic on the shared file must preserve B's unrelated selection"
    );
    server.shutdown();
}

#[test]
fn interleaved_project_switch_preserves_shared_owner_and_removed_owner_stays_invalid() {
    let temp = tempfile::tempdir().unwrap();
    let fixture = shared_owner_fixture(temp.path());
    let root = temp.path();
    let mut server = TestServer::launch();
    server.initialize(root, Value::Null);

    let select_a = RequestId::from("shared-owner-select-a".to_string());
    server.send_request(
        select_a.clone(),
        "pascal/selectProject",
        json!({
            "textDocument": {"uri": uri(&fixture.a_main)},
            "projectUri": uri(&fixture.a_project)
        }),
    );
    assert!(server.response(&select_a).error.is_none());

    let a_navigation = RequestId::from("shared-owner-a-navigation".to_string());
    server.send_request(
        a_navigation.clone(),
        "textDocument/declaration",
        navigation_params(&fixture.a_main, &fixture.main_source, "Run", 0),
    );
    assert_eq!(
        result_locations(server.response(&a_navigation))[0]["uri"],
        uri(&fixture.shared).to_string()
    );

    let shared_before_switch = RequestId::from("shared-owner-before-switch".to_string());
    server.send_request(
        shared_before_switch.clone(),
        "textDocument/declaration",
        navigation_params(&fixture.shared, &fixture.shared_source, "ConfigRoutine", 0),
    );
    assert_eq!(
        result_locations(server.response(&shared_before_switch))[0]["uri"],
        uri(&fixture.a_config).to_string()
    );

    let select_b = RequestId::from("shared-owner-select-b".to_string());
    server.send_request(
        select_b.clone(),
        "pascal/selectProject",
        json!({
            "textDocument": {"uri": uri(&fixture.b_main)},
            "projectUri": uri(&fixture.b_project)
        }),
    );
    assert!(server.response(&select_b).error.is_none());

    let b_navigation = RequestId::from("shared-owner-b-navigation".to_string());
    server.send_request(
        b_navigation.clone(),
        "textDocument/declaration",
        navigation_params(&fixture.b_main, &fixture.main_source, "Run", 0),
    );
    assert_eq!(
        result_locations(server.response(&b_navigation))[0]["uri"],
        uri(&fixture.shared).to_string()
    );

    let shared_navigation = RequestId::from("shared-owner-after-switch".to_string());
    server.send_request(
        shared_navigation.clone(),
        "textDocument/declaration",
        navigation_params(&fixture.shared, &fixture.shared_source, "ConfigRoutine", 0),
    );
    assert_eq!(
        result_locations(server.response(&shared_navigation))[0]["uri"],
        uri(&fixture.a_config).to_string(),
        "the shared unit must retain A's known owner after B's cache invalidation"
    );

    fs::remove_file(&fixture.a_project).unwrap();
    server.send_notification(
        "workspace/didChangeWatchedFiles",
        json!({"changes": [{"uri": uri(&fixture.a_project), "type": 3}]}),
    );
    let select_b_again = RequestId::from("shared-owner-reselect-b".to_string());
    server.send_request(
        select_b_again.clone(),
        "pascal/selectProject",
        json!({
            "textDocument": {"uri": uri(&fixture.b_main)},
            "projectUri": uri(&fixture.b_project)
        }),
    );
    assert!(server.response(&select_b_again).error.is_none());

    let invalid_shared_navigation = RequestId::from("invalid-shared-owner".to_string());
    server.send_request(
        invalid_shared_navigation.clone(),
        "textDocument/declaration",
        navigation_params(&fixture.shared, &fixture.shared_source, "ConfigRoutine", 0),
    );
    assert!(
        result_locations(server.response(&invalid_shared_navigation)).is_empty(),
        "a removed selected owner must not be replaced by B's importing context"
    );
    server.shutdown();
}

#[test]
fn workers_and_formatting_preserve_known_shared_owner_and_reject_removed_owner() {
    let temp = tempfile::tempdir().unwrap();
    let fixture = shared_owner_fixture(temp.path());
    let root = temp.path();
    write_file(
        &root.join(".lint4d.toml"),
        "[rules.naming]\nconstant_style = \"UPPER_CASE\"\n",
    );
    write_file(
        &root.join(".fmt4d.toml"),
        "[format]\nend_of_line = \"lf\"\n",
    );
    write_file(
        &fixture.a_project.parent().unwrap().join(".lint4d.toml"),
        "[rules.naming]\nconstant_style = \"UPPER_CASE\"\n",
    );
    write_file(
        &fixture.a_project.parent().unwrap().join(".fmt4d.toml"),
        "[format]\nend_of_line = \"crlf\"\n",
    );

    let mut server = TestServer::launch();
    server.initialize(root, Value::Null);
    let select_id = RequestId::from("worker-shared-owner-select".to_string());
    server.send_request(
        select_id.clone(),
        "pascal/selectProject",
        json!({
            "textDocument": {"uri": uri(&fixture.a_main)},
            "projectUri": uri(&fixture.a_project)
        }),
    );
    assert!(server.response(&select_id).error.is_none());
    let load_id = RequestId::from("worker-shared-owner-load".to_string());
    server.send_request(
        load_id.clone(),
        "textDocument/declaration",
        navigation_params(&fixture.a_main, &fixture.main_source, "Run", 0),
    );
    assert_eq!(
        result_locations(server.response(&load_id))[0]["uri"],
        uri(&fixture.shared).to_string()
    );

    let format_id = RequestId::from("worker-shared-owner-format".to_string());
    server.send_request(
        format_id.clone(),
        "textDocument/formatting",
        json!({
            "textDocument": {"uri": uri(&fixture.shared)},
            "options": {"tabSize": 2, "insertSpaces": true}
        }),
    );
    let format_response = server.response(&format_id);
    let format_uses_owner = format_response.error.is_none()
        && format_response
            .result
            .as_ref()
            .and_then(Value::as_array)
            .and_then(|edits| edits.first())
            .and_then(|edit| edit["newText"].as_str())
            .is_some_and(|text| text.contains("\r\n"));

    let start = position_of(&fixture.shared_source, "BadConst", 0);
    let end = Position::new(start.line, start.character + 8);
    let action_id = RequestId::from("worker-shared-owner-action".to_string());
    server.send_request(
        action_id.clone(),
        "textDocument/codeAction",
        json!({
            "textDocument": {"uri": uri(&fixture.shared)},
            "range": {"start": start, "end": end},
            "context": {
                "diagnostics": [{
                    "range": {"start": start, "end": end},
                    "severity": 2,
                    "code": "constant-naming",
                    "source": "lint4d",
                    "message": "naming violation"
                }],
                "only": ["quickfix"]
            }
        }),
    );
    let action_response = server.response(&action_id);
    let action_uses_owner = action_response.error.is_none()
        && action_response
            .result
            .as_ref()
            .and_then(Value::as_array)
            .and_then(|actions| actions.first())
            .is_some_and(|action| action["title"] == "Rename 'BadConst' to 'BAD_CONST'");

    fs::remove_file(&fixture.a_project).unwrap();
    server.send_notification(
        "workspace/didChangeWatchedFiles",
        json!({"changes": [{"uri": uri(&fixture.a_project), "type": 3}]}),
    );
    let rename_id = RequestId::from("worker-invalid-shared-owner".to_string());
    server.send_request(
        rename_id.clone(),
        "textDocument/rename",
        json!({
            "textDocument": {"uri": uri(&fixture.shared)},
            "position": start,
            "newName": "GoodConst"
        }),
    );
    let rename_response = server.response(&rename_id);
    let removed_owner_rejected = rename_response
        .error
        .as_ref()
        .is_some_and(|error| error.code == -32803 && error.message.contains("invalid"));
    assert!(
        format_uses_owner && action_uses_owner && removed_owner_rejected,
        "owner checks: format={format_response:?}, action={action_response:?}, rename={rename_response:?}"
    );
    server.shutdown();
}

#[test]
fn malformed_retained_owner_fails_closed_for_shared_document_rename() {
    let temp = tempfile::tempdir().unwrap();
    let fixture = shared_owner_fixture(temp.path());
    let root = temp.path();
    let mut server = TestServer::launch();
    server.initialize(root, Value::Null);

    let select_id = RequestId::from("malformed-owner-select-a".to_string());
    server.send_request(
        select_id.clone(),
        "pascal/selectProject",
        json!({
            "textDocument": {"uri": uri(&fixture.a_main)},
            "projectUri": uri(&fixture.a_project)
        }),
    );
    assert!(server.response(&select_id).error.is_none());

    let load_id = RequestId::from("malformed-owner-load-shared".to_string());
    server.send_request(
        load_id.clone(),
        "textDocument/declaration",
        navigation_params(&fixture.a_main, &fixture.main_source, "Run", 0),
    );
    assert_eq!(
        result_locations(server.response(&load_id))[0]["uri"],
        uri(&fixture.shared).to_string()
    );

    write_file(&fixture.a_project, "<Project>");
    server.send_notification(
        "workspace/didChangeWatchedFiles",
        json!({"changes": [{"uri": uri(&fixture.a_project), "type": 2}]}),
    );

    let position = position_of(&fixture.shared_source, "BadConst", 0);
    let rename_id = RequestId::from("malformed-owner-rename".to_string());
    server.send_request(
        rename_id.clone(),
        "textDocument/rename",
        json!({
            "textDocument": {"uri": uri(&fixture.shared)},
            "position": position,
            "newName": "GoodConst"
        }),
    );
    let response = server.response(&rename_id);
    let error = response
        .error
        .expect("malformed retained owner must reject rename");
    assert_eq!(error.code, -32803);
    assert!(
        error.message.contains("known project owner"),
        "rename error must identify retained-owner discovery failure: {error:?}"
    );
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
fn automatic_owner_is_rediscovered_when_a_second_owner_appears() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path().join("workspace");
    let app = root.join("app");
    let main = app.join("Main.pas");
    let project_a = app.join("A.dproj");
    let project_b = app.join("B.dproj");
    let source = "unit Main;\ninterface\nconst\n  BadConst = 1;\nimplementation\nend.\n";
    write_file(&main, source);
    write_file(
        &project_a,
        "<Project><PropertyGroup><MainSource>Main.pas</MainSource></PropertyGroup></Project>",
    );

    let mut server = TestServer::launch();
    server.initialize(&root, Value::Null);
    let initial_id = RequestId::from("automatic-owner-initial".to_string());
    server.send_request(
        initial_id.clone(),
        "pascal/projectContext",
        json!({"textDocument": {"uri": uri(&main)}}),
    );
    let initial = server.response(&initial_id);
    assert!(
        initial.error.is_none(),
        "initial context failed: {initial:?}"
    );
    assert_eq!(
        initial.result.as_ref().unwrap()["selectionMode"],
        "automatic"
    );
    assert_eq!(
        initial.result.as_ref().unwrap()["selectedProjectUri"],
        uri(&project_a).to_string()
    );

    write_file(
        &project_b,
        "<Project><PropertyGroup><MainSource>Main.pas</MainSource></PropertyGroup></Project>",
    );
    server.send_notification(
        "workspace/didChangeWatchedFiles",
        json!({"changes": [{"uri": uri(&project_b), "type": 1}]}),
    );

    let context_id = RequestId::from("automatic-owner-ambiguous".to_string());
    server.send_request(
        context_id.clone(),
        "pascal/projectContext",
        json!({"textDocument": {"uri": uri(&main)}}),
    );
    let context = server.response(&context_id);
    assert!(
        context.error.is_none(),
        "ambiguous context failed: {context:?}"
    );
    let context_result = context.result.as_ref().unwrap();
    assert_eq!(context_result["selectionMode"], "ambiguous");
    assert!(
        context_result["selectedProjectUri"].is_null(),
        "automatic discovery must not retain the old project: {context_result}"
    );

    let position = position_of(source, "BadConst", 0);
    let rename_id = RequestId::from("automatic-owner-ambiguous-rename".to_string());
    server.send_request(
        rename_id.clone(),
        "textDocument/rename",
        json!({
            "textDocument": {"uri": uri(&main)},
            "position": position,
            "newName": "GOOD_CONST"
        }),
    );
    let rename = server.response(&rename_id);
    let error = rename
        .error
        .expect("ambiguous automatic owner must refuse rename");
    assert_eq!(error.code, -32803);
    assert!(
        error.message.contains("ambiguous") || error.message.contains("incomplete"),
        "unexpected ambiguity refusal: {error:?}"
    );
    server.shutdown();
}

#[test]
fn automatic_owner_rechecks_nearest_scope_for_diagnostics_and_formatting() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path().join("workspace");
    let outer = root.join("outer");
    let inner = outer.join("inner");
    let main = inner.join("Main.pas");
    let outer_project = outer.join("Outer.dproj");
    let inner_project = inner.join("Inner.dproj");
    let source = "unit Main;\ninterface\nconst\n  BadConst = 1;\nimplementation\nprocedure Run;\nbegin\n  BadConst := 1;\nend;\nend.\n";
    write_file(&main, source);
    write_file(
        &outer_project,
        "<Project><PropertyGroup><MainSource>inner/Main.pas</MainSource></PropertyGroup></Project>",
    );
    write_file(
        &outer.join(".lint4d.toml"),
        "[rules.naming]\nconstant_style = \"PascalCase\"\n",
    );
    write_file(
        &outer.join(".fmt4d.toml"),
        "[format]\nend_of_line = \"lf\"\n",
    );

    let mut server = TestServer::launch();
    server.initialize(&root, Value::Null);
    server.send_notification(
        "textDocument/didOpen",
        json!({"textDocument": {"uri": uri(&main), "languageId": "pascal", "version": 1, "text": source}}),
    );
    let initial_diagnostics = diagnostics_for_uri(&mut server, &uri(&main));
    assert!(
        initial_diagnostics["diagnostics"]
            .as_array()
            .expect("initial diagnostics")
            .iter()
            .all(|diagnostic| diagnostic["code"] != "constant-naming"),
        "outer automatic project should use PascalCase: {initial_diagnostics}"
    );

    write_file(
        &inner_project,
        "<Project><PropertyGroup><MainSource>Main.pas</MainSource></PropertyGroup></Project>",
    );
    write_file(
        &inner.join(".lint4d.toml"),
        "[rules.naming]\nconstant_style = \"UPPER_CASE\"\n",
    );
    write_file(
        &inner.join(".fmt4d.toml"),
        "[format]\nend_of_line = \"crlf\"\n",
    );
    server.send_notification(
        "workspace/didChangeWatchedFiles",
        json!({"changes": [{"uri": uri(&inner_project), "type": 1}]}),
    );

    let context_id = RequestId::from("nearest-owner-context".to_string());
    server.send_request(
        context_id.clone(),
        "pascal/projectContext",
        json!({"textDocument": {"uri": uri(&main)}}),
    );
    let context = server.response(&context_id);
    assert!(
        context.error.is_none(),
        "nearest context failed: {context:?}"
    );
    let context_result = context.result.as_ref().unwrap();
    assert_eq!(context_result["selectionMode"], "automatic");
    assert_eq!(
        context_result["selectedProjectUri"],
        uri(&inner_project).to_string()
    );
    assert_eq!(context_result["scopeUri"], uri(&inner).to_string());

    let diagnostics = diagnostics_for_uri(&mut server, &uri(&main));
    assert!(
        diagnostics["diagnostics"]
            .as_array()
            .expect("updated diagnostics")
            .iter()
            .any(|diagnostic| diagnostic["code"] == "constant-naming"),
        "nearer project diagnostics were not recomputed: {diagnostics}"
    );

    let format_id = RequestId::from("nearest-owner-format".to_string());
    server.send_request(
        format_id.clone(),
        "textDocument/formatting",
        json!({
            "textDocument": {"uri": uri(&main)},
            "options": {"tabSize": 2, "insertSpaces": true}
        }),
    );
    let formatting = server.response(&format_id);
    assert!(
        formatting.error.is_none(),
        "nearest formatting failed: {formatting:?}"
    );
    assert!(
        formatting.result.as_ref().unwrap()[0]["newText"]
            .as_str()
            .expect("formatting text")
            .contains("\r\n"),
        "formatting did not use the nearer project's configuration: {formatting:?}"
    );
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
fn dynamic_watcher_registration_covers_repository_parent_configuration_fallbacks() {
    let temp = tempfile::tempdir().unwrap();
    let repository = temp.path().join("repository");
    let workspace = repository.join("nested-workspace");
    fs::create_dir_all(&workspace).expect("workspace directory");
    write_file(&repository.join(".git"), "gitdir: /outside/worktree\n");

    let mut server = TestServer::launch();
    server.initialize_with_watched_registration_and_relative_patterns(
        &workspace,
        Value::Null,
        true,
    );
    let registration = server.request("client/registerCapability");
    let watchers = registration.params["registrations"][0]["registerOptions"]["watchers"]
        .as_array()
        .expect("file watchers");
    assert!(
        watchers.iter().any(|watcher| {
            watcher["globPattern"]["baseUri"] == uri(&repository).to_string()
                && watcher["globPattern"]["pattern"] == ".lint4d.toml"
        }),
        "repository-parent lint config needs an explicit watcher: {watchers:?}"
    );
    assert!(
        watchers.iter().any(|watcher| {
            watcher["globPattern"]["baseUri"] == uri(&repository).to_string()
                && watcher["globPattern"]["pattern"] == ".fmt4d.toml"
        }),
        "repository-parent fmt config needs an explicit watcher: {watchers:?}"
    );
    server.send(Message::Response(Response::new_ok(
        registration.id,
        Value::Null,
    )));
    server.shutdown();
}

#[test]
fn relative_watcher_registration_uses_the_configuration_parent_as_base_uri() {
    let temp = tempfile::tempdir().unwrap();
    let repository = temp.path().join("repository");
    let workspace = repository.join("nested-workspace");
    fs::create_dir_all(&workspace).expect("workspace directory");
    write_file(&repository.join(".git"), "gitdir: /outside/worktree\n");

    let mut server = TestServer::launch();
    server.initialize_with_watched_registration_and_relative_patterns(
        &workspace,
        Value::Null,
        true,
    );
    let registration = server.request("client/registerCapability");
    let watchers = registration.params["registrations"][0]["registerOptions"]["watchers"]
        .as_array()
        .expect("file watchers");
    let lint_config = repository.join(".lint4d.toml");
    let watcher = watchers
        .iter()
        .find(|watcher| watcher["globPattern"]["pattern"] == ".lint4d.toml")
        .expect("repository-parent lint config watcher");
    assert_eq!(
        watcher["globPattern"]["baseUri"],
        uri(&repository).to_string(),
        "external configuration must be watched from its actual parent"
    );
    assert_eq!(watcher["globPattern"]["pattern"], ".lint4d.toml");
    assert!(
        !watchers
            .iter()
            .any(|watcher| { watcher["globPattern"] == lint_config.to_string_lossy().as_ref() })
    );
    server.send(Message::Response(Response::new_ok(
        registration.id,
        Value::Null,
    )));
    server.shutdown();
}

#[test]
fn watcher_registration_degrades_explicit_paths_without_relative_pattern_support() {
    let temp = tempfile::tempdir().unwrap();
    let repository = temp.path().join("repository");
    let workspace = repository.join("nested-workspace");
    fs::create_dir_all(&workspace).expect("workspace directory");
    write_file(&repository.join(".git"), "gitdir: /outside/worktree\n");

    let mut server = TestServer::launch();
    server.initialize_with_watched_registration_and_relative_patterns(
        &workspace,
        Value::Null,
        false,
    );
    let registration = server.request("client/registerCapability");
    let watchers = registration.params["registrations"][0]["registerOptions"]["watchers"]
        .as_array()
        .expect("file watchers");
    let lint_config = repository.join(".lint4d.toml");
    assert!(
        !watchers
            .iter()
            .any(|watcher| { watcher["globPattern"] == lint_config.to_string_lossy().as_ref() }),
        "unsupported relative patterns must not be encoded as absolute string globs"
    );
    server.send(Message::Response(Response::new_ok(
        registration.id,
        Value::Null,
    )));
    server.shutdown();
}

#[test]
fn dynamic_watcher_registration_adds_discovered_project_scope_candidates() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let project = root.join("app");
    let main = project.join("Main.pas");
    write_file(&main, "unit Main; interface implementation end.\n");
    write_file(
        &project.join("App.dproj"),
        "<Project><PropertyGroup><MainSource>Main.pas</MainSource></PropertyGroup></Project>",
    );
    write_file(&project.join("App.dpr"), "program App; begin end.\n");

    let mut server = TestServer::launch();
    server.initialize_with_watched_registration_and_relative_patterns(root, Value::Null, true);
    let initial_registration = server.request("client/registerCapability");
    server.send(Message::Response(Response::new_ok(
        initial_registration.id,
        Value::Null,
    )));

    let context_id = RequestId::from("project-scope-watchers".to_string());
    server.send_request(
        context_id.clone(),
        "pascal/projectContext",
        json!({"textDocument": {"uri": uri(&main)}}),
    );
    let context = server.response(&context_id);
    assert!(
        context.error.is_none(),
        "project context failed: {context:?}"
    );

    let update = server.request("client/registerCapability");
    let watchers = update.params["registrations"][0]["registerOptions"]["watchers"]
        .as_array()
        .expect("incremental file watchers");
    for filename in [".lint4d.toml", ".fmt4d.toml"] {
        assert!(
            watchers.iter().any(|watcher| {
                watcher["globPattern"]["baseUri"] == uri(&project).to_string()
                    && watcher["globPattern"]["pattern"] == filename
            }),
            "discovered project candidate needs an explicit watcher: {project:?}/{filename}; {watchers:?}"
        );
    }
    server.send(Message::Response(Response::new_ok(update.id, Value::Null)));
    server.shutdown();
}

#[test]
fn dynamic_watcher_registration_resolves_relative_configured_project_scope() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let project = root.join("app");
    let main = project.join("Main.pas");
    write_file(&main, "unit Main; interface implementation end.\n");
    write_file(
        &project.join("App.dproj"),
        "<Project><PropertyGroup><MainSource>Main.pas</MainSource></PropertyGroup></Project>",
    );

    let mut server = TestServer::launch();
    server.initialize_with_watched_registration_and_relative_patterns(
        root,
        json!({"projectFile": "app/App.dproj"}),
        true,
    );
    let registration = server.request("client/registerCapability");
    let watchers = registration.params["registrations"][0]["registerOptions"]["watchers"]
        .as_array()
        .expect("file watchers");
    for filename in [".lint4d.toml", ".fmt4d.toml"] {
        assert!(
            watchers.iter().any(|watcher| {
                watcher["globPattern"]["baseUri"] == uri(&project).to_string()
                    && watcher["globPattern"]["pattern"] == filename
            }),
            "relative configured project candidate needs an explicit watcher: {project:?}/{filename}; {watchers:?}"
        );
    }
    server.send(Message::Response(Response::new_ok(
        registration.id,
        Value::Null,
    )));
    server.shutdown();
}

#[test]
fn dynamic_watcher_registration_retries_rejected_paths_after_response() {
    let temp = tempfile::tempdir().unwrap();
    let repository = temp.path().join("repository");
    let workspace = repository.join("nested-workspace");
    let main = workspace.join("Main.pas");
    fs::create_dir_all(&workspace).expect("workspace directory");
    write_file(&repository.join(".git"), "gitdir: /outside/worktree\n");
    write_file(&main, "unit Main; interface implementation end.\n");

    let mut server = TestServer::launch();
    server.initialize_with_watched_registration_and_relative_patterns(
        &workspace,
        Value::Null,
        true,
    );
    let initial = server.request("client/registerCapability");
    let initial_watchers = initial.params["registrations"][0]["registerOptions"]["watchers"]
        .as_array()
        .expect("initial file watchers");
    assert!(initial_watchers.iter().any(|watcher| {
        watcher["globPattern"]["baseUri"] == uri(&repository).to_string()
            && watcher["globPattern"]["pattern"] == ".lint4d.toml"
    }));
    server.send(Message::Response(Response::new_err(
        initial.id,
        -32603,
        "watcher registration rejected".to_string(),
    )));

    let context_id = RequestId::from("retry-project-context".to_string());
    server.send_request(
        context_id.clone(),
        "pascal/projectContext",
        json!({"textDocument": {"uri": uri(&main)}}),
    );
    let context = server.response(&context_id);
    assert!(
        context.error.is_none(),
        "project context failed: {context:?}"
    );
    let retry = server.request("client/registerCapability");
    let retry_watchers = retry.params["registrations"][0]["registerOptions"]["watchers"]
        .as_array()
        .expect("retry file watchers");
    assert!(retry_watchers.iter().any(|watcher| {
        watcher["globPattern"]["baseUri"] == uri(&repository).to_string()
            && watcher["globPattern"]["pattern"] == ".lint4d.toml"
    }));
    server.send(Message::Response(Response::new_ok(retry.id, Value::Null)));
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
fn code_action_resolve_rejects_a_project_selection_switch() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path().join("fixture");
    let main = root.join("Main.pas");
    let project_a = root.join("A.dproj");
    let project_b = root.join("B.dproj");
    let source = "unit Main;\ninterface\nconst\n  badConst = 1;\nimplementation\nend.\n";
    write_file(&main, source);
    for project in [&project_a, &project_b] {
        write_file(
            project,
            "<Project><PropertyGroup><MainSource>Main.pas</MainSource></PropertyGroup></Project>",
        );
    }
    write_file(
        &root.join(".lint4d.toml"),
        "[rules.naming]\nconstant_style = \"UPPER_CASE\"\n",
    );
    let start = position_of(source, "badConst", 0);
    let end = Position::new(start.line, start.character + 8);

    let mut server = TestServer::launch();
    server.initialize_with_action_support(&root, Value::Null);
    let select_a_id = RequestId::from("select-project-a-for-action".to_string());
    server.send_request(
        select_a_id.clone(),
        "pascal/selectProject",
        json!({
            "textDocument": {"uri": uri(&main)},
            "projectUri": uri(&project_a)
        }),
    );
    assert!(server.response(&select_a_id).error.is_none());

    let action_id = RequestId::from("selection-switch-action".to_string());
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

    let select_b_id = RequestId::from("select-project-b-before-resolve".to_string());
    server.send_request(
        select_b_id.clone(),
        "pascal/selectProject",
        json!({
            "textDocument": {"uri": uri(&main)},
            "projectUri": uri(&project_b)
        }),
    );
    assert!(server.response(&select_b_id).error.is_none());

    let resolve_id = RequestId::from("selection-switch-resolve".to_string());
    server.send_request(resolve_id.clone(), "codeAction/resolve", action);
    let response = server.response(&resolve_id);
    assert!(
        response.error.is_some(),
        "project selection changes must reject a pending action: {response:?}"
    );
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
