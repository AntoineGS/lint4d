use std::collections::{HashSet, VecDeque};
#[cfg(target_os = "linux")]
use std::ffi::CString;
#[cfg(target_os = "linux")]
use std::fmt::Write as _;
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
#[cfg(feature = "test-support")]
use std::sync::mpsc::RecvTimeoutError;
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use lsp_server::{Message, Notification, Request, RequestId, Response};
use lsp_types::{Position, TextEdit, Url};
use pascal_core::FileInfo;
use pascal_lsp::workspace::{FileChange, Workspace, WorkspaceOptions};
use pascal_lsp::{NavigationTarget, ProjectContext, text};
use pascal_project::delphi_overrides::OverrideSession;
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
const IN_NONBLOCK: i32 = 0x800;

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
fn observed_open(path: &Path, operation: impl FnOnce()) -> bool {
    let fd = unsafe { inotify_init1(IN_NONBLOCK) };
    assert!(fd >= 0, "inotify_init1 failed");
    let pathname = CString::new(path.to_string_lossy().as_bytes()).expect("valid path");
    let watch = unsafe { inotify_add_watch(fd, pathname.as_ptr(), IN_OPEN) };
    assert!(watch >= 0, "inotify_add_watch failed");
    operation();

    let mut file = unsafe { std::fs::File::from_raw_fd(fd) };
    let mut events = [0_u8; 4096];
    match io::Read::read(&mut file, &mut events) {
        Ok(bytes) => {
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
                    return true;
                }
            }
            false
        }
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => false,
        Err(error) => panic!("read inotify events: {error}"),
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

fn test_workspace(roots: Vec<PathBuf>, options: WorkspaceOptions) -> Workspace {
    Workspace::with_override_session(roots, options, OverrideSession::new(None))
}

struct TestServer {
    child: Child,
    stdin: Option<ChildStdin>,
    messages: Receiver<io::Result<Option<Message>>>,
    pending: VecDeque<Message>,
    _environment: Option<TempDir>,
}

#[cfg(feature = "test-support")]
struct TestBarrier {
    entered: PathBuf,
    release: PathBuf,
}

#[cfg(feature = "test-support")]
struct TestDispatchLog {
    path: PathBuf,
}

impl TestServer {
    fn launch() -> Self {
        Self::launch_with_environment(tempfile::tempdir().expect("isolated server environment"))
    }

    fn launch_with_environment(environment: TempDir) -> Self {
        let mut server = Self::launch_with_environment_path(environment.path());
        server._environment = Some(environment);
        server
    }

    fn launch_with_environment_path(environment: &Path) -> Self {
        Self::launch_with_environment_path_and_variable(environment, None, None)
    }

    #[cfg(feature = "test-support")]
    fn launch_with_navigation_barrier(environment: TempDir) -> (Self, TestBarrier) {
        Self::launch_with_barrier(environment, "PASCAL_LSP_TEST_NAVIGATION_BARRIER")
    }

    #[cfg(feature = "test-support")]
    fn launch_with_navigation_barrier_and_filename_catalogue_limit(
        environment: TempDir,
        limit: usize,
    ) -> (Self, TestBarrier) {
        let barrier_directory = environment.path().join("analysis-barrier");
        fs::create_dir_all(&barrier_directory).expect("barrier directory");
        let barrier = TestBarrier {
            entered: barrier_directory.join("entered"),
            release: barrier_directory.join("release"),
        };
        let barrier_value = format!(
            "{}|{}",
            barrier.entered.display(),
            barrier.release.display()
        );
        let limit_value = limit.to_string();
        let mut server = Self::launch_test_server_with_environment_path_and_variables(
            environment.path(),
            [
                ("PASCAL_LSP_TEST_NAVIGATION_BARRIER", barrier_value.as_str()),
                (
                    "PASCAL_LSP_TEST_FILENAME_CATALOGUE_ENTRIES",
                    limit_value.as_str(),
                ),
            ],
        );
        server._environment = Some(environment);
        (server, barrier)
    }

    #[cfg(feature = "test-support")]
    fn launch_with_navigation_barrier_and_dispatch_log(
        environment: TempDir,
    ) -> (Self, TestBarrier, TestDispatchLog) {
        let barrier_directory = environment.path().join("analysis-barrier");
        let dispatch_directory = environment.path().join("analysis-dispatch");
        fs::create_dir_all(&barrier_directory).expect("barrier directory");
        fs::create_dir_all(&dispatch_directory).expect("dispatch directory");
        let barrier = TestBarrier {
            entered: barrier_directory.join("entered"),
            release: barrier_directory.join("release"),
        };
        let dispatch = TestDispatchLog {
            path: dispatch_directory.join("entries"),
        };
        let navigation_value = format!(
            "{}|{}",
            barrier.entered.display(),
            barrier.release.display()
        );
        let dispatch_value = dispatch.path.display().to_string();
        let mut server = Self::launch_test_server_with_environment_path_and_variables(
            environment.path(),
            [
                (
                    "PASCAL_LSP_TEST_NAVIGATION_BARRIER",
                    navigation_value.as_str(),
                ),
                ("PASCAL_LSP_TEST_DISPATCH_LOG", dispatch_value.as_str()),
            ],
        );
        server._environment = Some(environment);
        (server, barrier, dispatch)
    }

    #[cfg(feature = "test-support")]
    fn launch_with_navigation_and_diagnostics_barriers_and_dispatch_log(
        environment: TempDir,
    ) -> (Self, TestBarrier, TestBarrier, TestDispatchLog) {
        let barrier_directory = environment.path().join("analysis-barrier");
        let dispatch_directory = environment.path().join("analysis-dispatch");
        fs::create_dir_all(&barrier_directory).expect("barrier directory");
        fs::create_dir_all(&dispatch_directory).expect("dispatch directory");
        let navigation = TestBarrier {
            entered: barrier_directory.join("navigation.entered"),
            release: barrier_directory.join("navigation.release"),
        };
        let diagnostics = TestBarrier {
            entered: barrier_directory.join("diagnostics.entered"),
            release: barrier_directory.join("diagnostics.release"),
        };
        let dispatch = TestDispatchLog {
            path: dispatch_directory.join("entries"),
        };
        let navigation_value = format!(
            "{}|{}",
            navigation.entered.display(),
            navigation.release.display()
        );
        let diagnostics_value = format!(
            "{}|{}",
            diagnostics.entered.display(),
            diagnostics.release.display()
        );
        let dispatch_value = dispatch.path.display().to_string();
        let mut server = Self::launch_test_server_with_environment_path_and_variables(
            environment.path(),
            [
                (
                    "PASCAL_LSP_TEST_NAVIGATION_BARRIER",
                    navigation_value.as_str(),
                ),
                (
                    "PASCAL_LSP_TEST_DIAGNOSTICS_BARRIER",
                    diagnostics_value.as_str(),
                ),
                ("PASCAL_LSP_TEST_DISPATCH_LOG", dispatch_value.as_str()),
            ],
        );
        server._environment = Some(environment);
        (server, navigation, diagnostics, dispatch)
    }

    #[cfg(feature = "test-support")]
    fn launch_with_navigation_and_formatting_barriers_and_dispatch_log(
        environment: TempDir,
    ) -> (Self, TestBarrier, TestBarrier, TestDispatchLog) {
        let barrier_directory = environment.path().join("analysis-barrier");
        let dispatch_directory = environment.path().join("analysis-dispatch");
        fs::create_dir_all(&barrier_directory).expect("barrier directory");
        fs::create_dir_all(&dispatch_directory).expect("dispatch directory");
        let navigation = TestBarrier {
            entered: barrier_directory.join("navigation.entered"),
            release: barrier_directory.join("navigation.release"),
        };
        let formatting = TestBarrier {
            entered: barrier_directory.join("formatting.entered"),
            release: barrier_directory.join("formatting.release"),
        };
        let dispatch = TestDispatchLog {
            path: dispatch_directory.join("entries"),
        };
        let navigation_value = format!(
            "{}|{}",
            navigation.entered.display(),
            navigation.release.display()
        );
        let formatting_value = format!(
            "{}|{}",
            formatting.entered.display(),
            formatting.release.display()
        );
        let dispatch_value = dispatch.path.display().to_string();
        let mut server = Self::launch_test_server_with_environment_path_and_variables(
            environment.path(),
            [
                (
                    "PASCAL_LSP_TEST_NAVIGATION_BARRIER",
                    navigation_value.as_str(),
                ),
                (
                    "PASCAL_LSP_TEST_FORMATTING_BARRIER",
                    formatting_value.as_str(),
                ),
                ("PASCAL_LSP_TEST_DISPATCH_LOG", dispatch_value.as_str()),
            ],
        );
        server._environment = Some(environment);
        (server, navigation, formatting, dispatch)
    }

    #[cfg(feature = "test-support")]
    fn launch_with_formatting_barrier(environment: TempDir) -> (Self, TestBarrier) {
        Self::launch_with_barrier(environment, "PASCAL_LSP_TEST_FORMATTING_BARRIER")
    }

    #[cfg(feature = "test-support")]
    fn launch_with_diagnostics_barrier(environment: TempDir) -> (Self, TestBarrier) {
        Self::launch_with_barrier(environment, "PASCAL_LSP_TEST_DIAGNOSTICS_BARRIER")
    }

    #[cfg(feature = "test-support")]
    fn launch_with_selection_barrier(environment: TempDir) -> (Self, TestBarrier) {
        Self::launch_with_barrier(environment, "PASCAL_LSP_TEST_SELECTION_BARRIER")
    }

    #[cfg(feature = "test-support")]
    fn launch_with_completion_resolution_barrier(environment: TempDir) -> (Self, TestBarrier) {
        Self::launch_with_barrier(environment, "PASCAL_LSP_TEST_COMPLETION_RESOLUTION_BARRIER")
    }

    #[cfg(feature = "test-support")]
    fn launch_with_barrier(environment: TempDir, variable: &str) -> (Self, TestBarrier) {
        let barrier_directory = environment.path().join("analysis-barrier");
        fs::create_dir_all(&barrier_directory).expect("barrier directory");
        let barrier = TestBarrier {
            entered: barrier_directory.join("entered"),
            release: barrier_directory.join("release"),
        };
        let value = format!(
            "{}|{}",
            barrier.entered.display(),
            barrier.release.display()
        );
        let mut server = Self::launch_test_server_with_environment_path_and_variable(
            environment.path(),
            Some(variable),
            Some(value.as_str()),
        );
        server._environment = Some(environment);
        (server, barrier)
    }

    #[cfg(feature = "test-support")]
    fn launch_test_server_with_environment_path_and_variable(
        environment: &Path,
        variable: Option<&str>,
        value: Option<&str>,
    ) -> Self {
        Self::launch_test_server_with_environment_path_and_variables(
            environment,
            variable.into_iter().zip(value),
        )
    }

    #[cfg(feature = "test-support")]
    fn launch_test_server_with_environment_path_and_variables<'a>(
        environment: &Path,
        variables: impl IntoIterator<Item = (&'a str, &'a str)>,
    ) -> Self {
        let executable = env!("CARGO_BIN_EXE_pascal-lsp-test-server");
        let child = Command::new(executable)
            .arg("--stdio")
            .env("HOME", environment.join("home"))
            .env("XDG_CONFIG_HOME", environment.join("config"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .envs(variables)
            .spawn()
            .expect("launch pascal-lsp test server");
        Self::from_child(child)
    }

    fn launch_with_environment_path_and_variable(
        environment: &Path,
        variable: Option<&str>,
        value: Option<&str>,
    ) -> Self {
        let executable = env!("CARGO_BIN_EXE_pascal-lsp");
        let child = Command::new(executable)
            .arg("--stdio")
            .env("HOME", environment.join("home"))
            .env("XDG_CONFIG_HOME", environment.join("config"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .envs(variable.into_iter().zip(value))
            .spawn()
            .expect("launch pascal-lsp");
        Self::from_child(child)
    }

    fn from_child(mut child: Child) -> Self {
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
            _environment: None,
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

    #[cfg(feature = "test-support")]
    fn response_with_timeout(
        &mut self,
        expected_id: &RequestId,
        timeout: Duration,
    ) -> Option<Response> {
        if let Some(index) = self.pending.iter().position(
            |message| matches!(message, Message::Response(response) if &response.id == expected_id),
        ) {
            return match self.pending.remove(index).expect("pending response") {
                Message::Response(response) => Some(response),
                _ => unreachable!("pending response predicate"),
            };
        }
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match self.messages.recv_timeout(remaining) {
                Ok(Ok(Some(Message::Response(response)))) if &response.id == expected_id => {
                    return Some(response);
                }
                Ok(Ok(Some(message))) => self.pending.push_back(message),
                Ok(Ok(None)) | Err(RecvTimeoutError::Disconnected) => return None,
                Ok(Err(error)) => panic!("failed reading response: {error}"),
                Err(RecvTimeoutError::Timeout) => return None,
            }
        }
        None
    }

    #[cfg(feature = "test-support")]
    fn assert_no_response(&mut self, expected_id: &RequestId) {
        assert!(
            !self
                .pending
                .iter()
                .any(|message| matches!(message, Message::Response(response) if &response.id == expected_id)),
            "duplicate response for {expected_id:?}"
        );
        let deadline = Instant::now() + Duration::from_millis(100);
        while Instant::now() < deadline {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match self.messages.recv_timeout(remaining) {
                Ok(Ok(Some(Message::Response(response)))) if &response.id == expected_id => {
                    panic!("duplicate response for {expected_id:?}: {response:?}");
                }
                Ok(Ok(Some(message))) => self.pending.push_back(message),
                Ok(Ok(None)) => return,
                Ok(Err(error)) => panic!("failed reading duplicate-response check: {error}"),
                Err(RecvTimeoutError::Timeout) => return,
                Err(RecvTimeoutError::Disconnected) => return,
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

    #[cfg(feature = "test-support")]
    fn diagnostic_with_timeout(&mut self, expected: &Url, timeout: Duration) -> Option<Value> {
        let expected = expected.to_string();
        if let Some(index) = self.pending.iter().position(|message| {
            matches!(
                message,
                Message::Notification(notification)
                    if notification.method == "textDocument/publishDiagnostics"
                        && notification.params["uri"] == expected
            )
        }) {
            return match self.pending.remove(index).expect("pending diagnostics") {
                Message::Notification(notification) => Some(notification.params),
                _ => unreachable!("pending diagnostics predicate"),
            };
        }
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match self.messages.recv_timeout(remaining) {
                Ok(Ok(Some(Message::Notification(notification))))
                    if notification.method == "textDocument/publishDiagnostics"
                        && notification.params["uri"] == expected =>
                {
                    return Some(notification.params);
                }
                Ok(Ok(Some(message))) => self.pending.push_back(message),
                Ok(Ok(None)) | Err(RecvTimeoutError::Disconnected) => return None,
                Ok(Err(error)) => panic!("failed reading diagnostic: {error}"),
                Err(RecvTimeoutError::Timeout) => return None,
            }
        }
        None
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

    fn initialize_with_folding_capabilities(
        &mut self,
        root: &Path,
        folding_capabilities: Value,
    ) -> Value {
        let root_uri = Url::from_file_path(root).expect("workspace URI");
        let id = RequestId::from("initialize".to_string());
        self.send_request(
            id.clone(),
            "initialize",
            json!({
                "processId": null,
                "rootUri": root_uri,
                "capabilities": {
                    "textDocument": {"foldingRange": folding_capabilities}
                }
            }),
        );
        let response = self.response(&id);
        assert!(response.error.is_none(), "initialize failed: {response:?}");
        self.send_notification("initialized", json!({}));
        response.result.expect("initialize result")
    }

    fn initialize_with_workspace_folders(
        &mut self,
        root: &Path,
        folders: &[&Path],
        initialization_options: Value,
    ) -> Value {
        let root_uri = Url::from_file_path(root).expect("workspace URI");
        let workspace_folders = folders
            .iter()
            .map(|folder| {
                json!({
                    "uri": uri(folder),
                    "name": folder.display().to_string(),
                })
            })
            .collect::<Vec<_>>();
        let id = RequestId::from("initialize".to_string());
        self.send_request(
            id.clone(),
            "initialize",
            json!({
                "processId": null,
                "rootUri": root_uri,
                "workspaceFolders": workspace_folders,
                "initializationOptions": initialization_options,
                "capabilities": {
                    "workspace": {
                        "workspaceFolders": true,
                        "didChangeWatchedFiles": {"dynamicRegistration": false}
                    }
                }
            }),
        );
        let response = self.response(&id);
        assert!(response.error.is_none(), "initialize failed: {response:?}");
        self.send_notification("initialized", json!({}));
        response.result.expect("initialize result")
    }

    fn initialize_with_hierarchical_document_symbols(&mut self, root: &Path) -> Value {
        let root_uri = Url::from_file_path(root).expect("workspace URI");
        let id = RequestId::from("initialize".to_string());
        self.send_request(
            id.clone(),
            "initialize",
            json!({
                "processId": null,
                "rootUri": root_uri,
                "capabilities": {
                    "textDocument": {
                        "documentSymbol": {
                            "hierarchicalDocumentSymbolSupport": true
                        }
                    }
                }
            }),
        );
        let response = self.response(&id);
        assert!(response.error.is_none(), "initialize failed: {response:?}");
        self.send_notification("initialized", json!({}));
        response.result.expect("initialize result")
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

    fn initialize_with_completion_resolve_properties(
        &mut self,
        root: &Path,
        properties: Value,
        documentation_formats: Value,
    ) -> Value {
        self.initialize_with_completion_capabilities(root, None, properties, documentation_formats)
    }

    fn initialize_with_completion_capabilities(
        &mut self,
        root: &Path,
        snippet_support: Option<bool>,
        properties: Value,
        documentation_formats: Value,
    ) -> Value {
        let root_uri = Url::from_file_path(root).expect("workspace URI");
        let id = RequestId::from("initialize".to_string());
        let mut completion_item = json!({
            "documentationFormat": documentation_formats,
            "resolveSupport": {"properties": properties}
        });
        if let Some(snippet_support) = snippet_support {
            completion_item["snippetSupport"] = json!(snippet_support);
        }
        self.send_request(
            id.clone(),
            "initialize",
            json!({
                "processId": null,
                "rootUri": root_uri,
                "initializationOptions": null,
                "capabilities": {
                    "general": {"positionEncodings": ["utf-16"]},
                    "textDocument": {
                        "synchronization": {"dynamicRegistration": false, "didSave": true},
                        "completion": {
                            "completionItem": completion_item
                        }
                    },
                    "workspace": {"workspaceFolders": true}
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

#[cfg(feature = "test-support")]
impl TestBarrier {
    fn wait_until_entered(&self) {
        self.wait_for_entries(1);
    }

    fn wait_for_entries(&self, expected: usize) {
        let deadline = Instant::now() + IO_TIMEOUT;
        while Instant::now() < deadline {
            let entries = match fs::read(&self.entered) {
                Ok(contents) => contents.len().max(1),
                Err(error) if error.kind() == io::ErrorKind::NotFound => 0,
                Err(error) => panic!("could not inspect analysis barrier: {error}"),
            };
            if entries >= expected {
                return;
            }
            thread::sleep(Duration::from_millis(5));
        }
        panic!("expected {expected} analysis workers at the test barrier");
    }

    fn release(&self) {
        fs::write(&self.release, b"release").expect("release analysis barrier");
    }
}

#[cfg(feature = "test-support")]
impl TestDispatchLog {
    fn wait_for_entries(&self, expected: usize) -> Vec<u8> {
        let deadline = Instant::now() + IO_TIMEOUT;
        while Instant::now() < deadline {
            if let Ok(entries) = fs::read(&self.path) {
                if entries.len() >= expected {
                    return entries;
                }
            }
            thread::sleep(Duration::from_millis(5));
        }
        panic!("expected {expected} analysis dispatches at the test log");
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

#[cfg(feature = "test-support")]
fn wait_for_path(path: &Path) {
    let deadline = Instant::now() + IO_TIMEOUT;
    while Instant::now() < deadline {
        if path.exists() {
            return;
        }
        thread::sleep(Duration::from_millis(5));
    }
    panic!("expected path to be created: {}", path.display());
}

fn uri(path: &Path) -> Url {
    Url::from_file_path(path).expect("file URI")
}

fn decoded_semantic_tokens(
    result: &Value,
    token_types: &[&str],
) -> Vec<(u32, u32, u32, String, u32)> {
    let data = result["data"].as_array().expect("semantic token data");
    let mut line = 0;
    let mut character = 0;
    let mut tokens = Vec::with_capacity(data.len() / 5);
    for chunk in data.chunks_exact(5) {
        let delta_line = chunk[0].as_u64().expect("delta line") as u32;
        let delta_start = chunk[1].as_u64().expect("delta start") as u32;
        line += delta_line;
        character = if delta_line == 0 {
            character + delta_start
        } else {
            delta_start
        };
        let token_type = chunk[3].as_u64().expect("token type") as usize;
        tokens.push((
            line,
            character,
            chunk[2].as_u64().expect("token length") as u32,
            token_types[token_type].to_string(),
            chunk[4].as_u64().expect("token modifiers") as u32,
        ));
    }
    tokens
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

fn position_after(source: &str, needle: &str, occurrence: usize) -> Position {
    let start = position_of(source, needle, occurrence);
    Position::new(
        start.line,
        start.character + needle.encode_utf16().count() as u32,
    )
}

fn apply_completion_item(source: &str, item: &Value) -> String {
    let mut edits = Vec::new();
    let primary: TextEdit =
        serde_json::from_value(item["textEdit"].clone()).expect("completion primary text edit");
    edits.push(primary);
    if let Some(additional) = item["additionalTextEdits"].as_array() {
        edits.extend(
            additional
                .iter()
                .cloned()
                .map(|edit| serde_json::from_value(edit).expect("completion additional text edit")),
        );
    }
    let mut byte_edits = edits
        .into_iter()
        .map(|edit| {
            let start = text::position_to_offset(source, edit.range.start)
                .expect("completion edit start is a UTF-16 boundary");
            let end = text::position_to_offset(source, edit.range.end)
                .expect("completion edit end is a UTF-16 boundary");
            (start, end, edit.new_text)
        })
        .collect::<Vec<_>>();
    byte_edits.sort_by(|left, right| right.0.cmp(&left.0).then(right.1.cmp(&left.1)));
    let mut result = source.to_owned();
    for (start, end, new_text) in byte_edits {
        result.replace_range(start..end, &new_text);
    }
    result
}

fn expand_lsp_snippet(snippet: &str) -> String {
    let mut expanded = String::with_capacity(snippet.len());
    let bytes = snippet.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'\\' => {
                if let Some(next) = snippet[index + 1..].chars().next() {
                    expanded.push(next);
                    index += 1 + next.len_utf8();
                } else {
                    expanded.push('\\');
                    index += 1;
                }
            }
            b'$' if bytes.get(index + 1) == Some(&b'0') => index += 2,
            b'$' if bytes.get(index + 1) == Some(&b'{') => {
                let mut end = index + 2;
                let mut escaped = false;
                while end < bytes.len() {
                    if escaped {
                        escaped = false;
                    } else if bytes[end] == b'\\' {
                        escaped = true;
                    } else if bytes[end] == b'}' {
                        break;
                    }
                    end += 1;
                }
                if end >= bytes.len() {
                    expanded.push('$');
                    index += 1;
                    continue;
                }
                let body = &snippet[index + 2..end];
                let default = body.split_once(':').map_or(body, |(_, default)| default);
                expanded.push_str(&unescape_lsp_snippet_literal(default));
                index = end + 1;
            }
            _ => {
                let character = snippet[index..].chars().next().expect("snippet character");
                expanded.push(character);
                index += character.len_utf8();
            }
        }
    }
    expanded
}

fn unescape_lsp_snippet_literal(value: &str) -> String {
    let mut unescaped = String::with_capacity(value.len());
    let mut escaped = false;
    for character in value.chars() {
        if escaped {
            unescaped.push(character);
            escaped = false;
        } else if character == '\\' {
            escaped = true;
        } else {
            unescaped.push(character);
        }
    }
    if escaped {
        unescaped.push('\\');
    }
    unescaped
}

fn apply_expanded_completion_item(source: &str, item: &Value) -> String {
    let mut expanded = item.clone();
    let snippet = expanded["textEdit"]["newText"]
        .as_str()
        .expect("snippet text");
    expanded["textEdit"]["newText"] = json!(expand_lsp_snippet(snippet));
    apply_completion_item(source, &expanded)
}

fn final_qualified_type_position(source: &str, qualified_name: &str) -> Position {
    let start = position_of(source, qualified_name, 0);
    let prefix = qualified_name
        .rsplit_once('.')
        .map_or(0, |(prefix, _)| prefix.len() + 1);
    Position::new(start.line, start.character + prefix as u32)
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

#[cfg(feature = "test-support")]
fn assert_queue_overflow(response: Response) {
    let error = response.error.expect("analysis queue overflow error");
    assert_eq!(error.code, -32803);
    assert_eq!(error.message, "analysis queue is full; retry the request");
}

fn location_signature(location: &Value) -> (String, u32, u32, u32, u32) {
    (
        location["uri"].as_str().unwrap_or_default().to_owned(),
        location["range"]["start"]["line"].as_u64().unwrap() as u32,
        location["range"]["start"]["character"].as_u64().unwrap() as u32,
        location["range"]["end"]["line"].as_u64().unwrap() as u32,
        location["range"]["end"]["character"].as_u64().unwrap() as u32,
    )
}

fn range_signature(location: &Value) -> (u32, u32, u32, u32) {
    (
        location["range"]["start"]["line"].as_u64().unwrap() as u32,
        location["range"]["start"]["character"].as_u64().unwrap() as u32,
        location["range"]["end"]["line"].as_u64().unwrap() as u32,
        location["range"]["end"]["character"].as_u64().unwrap() as u32,
    )
}

fn expected_location_signature(
    path: &Path,
    source: &str,
    needle: &str,
    occurrence: usize,
) -> (String, u32, u32, u32, u32) {
    let start = position_of(source, needle, occurrence);
    (
        uri(path).to_string(),
        start.line,
        start.character,
        start.line,
        start.character + needle.encode_utf16().count() as u32,
    )
}

fn assert_exact_location_signatures(
    actual: &[Value],
    mut expected: Vec<(String, u32, u32, u32, u32)>,
) {
    let mut actual = actual.iter().map(location_signature).collect::<Vec<_>>();
    actual.sort();
    expected.sort();
    assert_eq!(actual, expected);
}

fn write_file(path: &Path, source: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create source directory");
    }
    fs::write(path, source).expect("write Pascal source");
}

fn workspace_symbol_source(unit_name: &str, variable_count: usize) -> String {
    let mut source = format!("unit {unit_name};\ninterface\nvar\n");
    for index in 0..variable_count {
        source.push_str(&format!("  Symbol{index}: Integer;\n"));
    }
    source.push_str("implementation\nend.\n");
    source
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

fn assert_exact_workspace_edit(
    edit: &Value,
    mut expected: Vec<(String, Position, Position, String)>,
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
    actual.sort();
    expected.sort();
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

#[cfg(unix)]
#[test]
fn delphi_overrides_user_config_navigates_to_native_source() {
    let environment = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let sdk = tempfile::tempdir().unwrap();
    let main = root.path().join("Main.pas");
    let provider = sdk.path().join("source/Provider.pas");
    let source = "unit Main;\ninterface\nuses Provider;\nimplementation\nend.\n";
    write_file(&main, source);
    write_file(&provider, "unit Provider; interface implementation end.");
    write_file(
        &root.path().join("Main.dproj"),
        r#"
<Project><PropertyGroup><MainSource>Main.pas</MainSource>
<DCC_UnitSearchPath>$(BDS)\source</DCC_UnitSearchPath>
</PropertyGroup></Project>
"#,
    );
    write_file(
        &environment.path().join("config/delphi-tools/config.toml"),
        &format!(
            "[properties]\nBDS = 'C:\\SDK'\n[[path_mappings]]\nfrom = 'C:\\SDK'\nto = '{}'\n",
            sdk.path().display()
        ),
    );
    let mut server = TestServer::launch_with_environment(environment);
    server.initialize(root.path(), Value::Null);
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
    let id = RequestId::from("mapped-definition".to_string());
    server.send_request(
        id.clone(),
        "textDocument/definition",
        json!({
            "textDocument": {"uri": uri(&main)},
            "position": {"line": 2, "character": 5}
        }),
    );
    let response = server.response(&id);
    assert!(response.error.is_none(), "{response:?}");
    let value = response.result.unwrap();
    let locations: Vec<lsp_types::Location> = serde_json::from_value(value).unwrap();
    assert_eq!(locations.len(), 1);
    assert_eq!(locations[0].uri, uri(&provider));
    server.shutdown();
    assert_eq!(fs::read_to_string(main).unwrap(), source);
}

#[cfg(unix)]
#[test]
fn automatic_mapped_dependency_did_open_retains_requesting_project_owner() {
    let environment = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let sdk = tempfile::tempdir().unwrap();
    let conflicting_sdk = tempfile::tempdir().unwrap();
    let main = root.path().join("Main.pas");
    let provider = sdk.path().join("Provider.pas");
    let helper = sdk.path().join("Helpers/Helper.pas");
    let source = "unit Main;\ninterface\nuses Provider;\nimplementation\nprocedure Run;\nbegin\n  ProviderRoutine;\nend;\nend.\n";
    let provider_source = "unit Provider;\ninterface\nuses Helper;\nprocedure ProviderRoutine;\nimplementation\nprocedure ProviderRoutine;\nbegin\n  HelperRoutine;\nend;\nend.\n";
    let helper_source = "unit Helper;\ninterface\nprocedure HelperRoutine;\nimplementation\nprocedure HelperRoutine; begin end;\nend.\n";
    write_file(&main, source);
    write_file(&provider, provider_source);
    write_file(&helper, helper_source);
    write_file(
        &root.path().join("App.dproj"),
        "<Project><PropertyGroup><MainSource>Main.pas</MainSource><DCC_UnitSearchPath>C:\\SDK;C:\\SDK\\Helpers</DCC_UnitSearchPath></PropertyGroup></Project>",
    );
    write_file(
        &sdk.path().join("Sdk.dproj"),
        "<Project><PropertyGroup><MainSource>Provider.pas</MainSource><DCC_UnitSearchPath>C:\\SDK\\Wrong</DCC_UnitSearchPath></PropertyGroup></Project>",
    );
    write_file(
        &root.path().join(".delphi-tools.local.toml"),
        &format!(
            "[[path_mappings]]\nfrom = 'C:\\SDK'\nto = '{}'\n",
            sdk.path().display()
        ),
    );
    write_file(
        &sdk.path().join(".delphi-tools.local.toml"),
        &format!(
            "[[path_mappings]]\nfrom = 'C:\\SDK'\nto = '{}'\n",
            conflicting_sdk.path().display()
        ),
    );

    let mut server = TestServer::launch_with_environment(environment);
    server.initialize(root.path(), Value::Null);
    let definition_id = RequestId::from("automatic-mapped-provider".to_string());
    server.send_request(
        definition_id.clone(),
        "textDocument/definition",
        navigation_params(&main, source, "ProviderRoutine", 0),
    );
    let definition = result_locations(server.response(&definition_id));
    assert_eq!(definition.len(), 1);
    assert_eq!(definition[0]["uri"], uri(&provider).to_string());

    server.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri(&provider),
                "languageId": "pascal",
                "version": 1,
                "text": provider_source
            }
        }),
    );
    let helper_id = RequestId::from("automatic-mapped-helper-after-open".to_string());
    server.send_request(
        helper_id.clone(),
        "textDocument/definition",
        navigation_params(&provider, provider_source, "HelperRoutine", 0),
    );
    let helper_locations = result_locations(server.response(&helper_id));
    assert_eq!(helper_locations.len(), 1);
    assert_eq!(helper_locations[0]["uri"], uri(&helper).to_string());
    server.shutdown();
}

#[cfg(unix)]
#[test]
fn delphi_overrides_workspace_and_project_precedence_survives_project_switching() {
    let environment = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let workspace_sdk = tempfile::tempdir().unwrap();
    let project_sdk_a = tempfile::tempdir().unwrap();
    let project_sdk_b = tempfile::tempdir().unwrap();
    let user_sdk = tempfile::tempdir().unwrap();
    let app = root.path().join("app");
    let app_main = app.join("Main.pas");
    let workspace_main = root.path().join("WorkspaceMain.pas");
    let workspace_provider = workspace_sdk.path().join("source/Provider.pas");
    let project_provider_a = project_sdk_a.path().join("source/Provider.pas");
    let project_provider_b = project_sdk_b.path().join("source/Provider.pas");
    let user_provider = user_sdk.path().join("source/Provider.pas");
    let main_source = "unit Main;\ninterface\nuses Provider;\nimplementation\nprocedure Run;\nbegin\n  ProviderRoutine;\nend;\nend.\n";
    let provider_source = "unit Provider;\ninterface\nprocedure ProviderRoutine;\nimplementation\nprocedure ProviderRoutine; begin end;\nend.\n";
    write_file(&app_main, main_source);
    write_file(&workspace_main, main_source);
    for provider in [
        &workspace_provider,
        &project_provider_a,
        &project_provider_b,
        &user_provider,
    ] {
        write_file(provider, provider_source);
    }
    write_file(
        &root.path().join("Workspace.dproj"),
        "<Project><PropertyGroup><MainSource>WorkspaceMain.pas</MainSource><DCC_UnitSearchPath>C:\\SDK\\source</DCC_UnitSearchPath></PropertyGroup></Project>",
    );
    for project in ["A", "B"] {
        write_file(
            &app.join(format!("{project}.dproj")),
            &format!(
                "<Project><PropertyGroup><MainSource>Main.pas</MainSource><DCC_UnitSearchPath>C:\\SDK_{project}\\source</DCC_UnitSearchPath></PropertyGroup></Project>"
            ),
        );
    }
    write_file(
        &environment.path().join("config/delphi-tools/config.toml"),
        &format!(
            "[[path_mappings]]\nfrom = 'C:\\SDK'\nto = '{}'\n[[path_mappings]]\nfrom = 'C:\\SDK_A'\nto = '{}'\n[[path_mappings]]\nfrom = 'C:\\SDK_B'\nto = '{}'\n",
            user_sdk.path().display(),
            user_sdk.path().display(),
            workspace_sdk.path().display()
        ),
    );
    write_file(
        &root.path().join(".delphi-tools.local.toml"),
        &format!(
            "[[path_mappings]]\nfrom = 'C:\\SDK'\nto = '{}'\n[[path_mappings]]\nfrom = 'C:\\SDK_A'\nto = '{}'\n[[path_mappings]]\nfrom = 'C:\\SDK_B'\nto = '{}'\n",
            workspace_sdk.path().display(),
            workspace_sdk.path().display(),
            user_sdk.path().display()
        ),
    );
    let mut lower_server = TestServer::launch_with_environment_path(environment.path());
    lower_server.initialize(root.path(), Value::Null);

    let lower_workspace_id = RequestId::from("lower-workspace-precedence".to_string());
    lower_server.send_request(
        lower_workspace_id.clone(),
        "textDocument/definition",
        navigation_params(&workspace_main, main_source, "Provider", 0),
    );
    let lower_workspace_locations = result_locations(lower_server.response(&lower_workspace_id));
    assert_eq!(lower_workspace_locations.len(), 1);
    assert_eq!(
        lower_workspace_locations[0]["uri"],
        uri(&workspace_provider).to_string()
    );

    let lower_select_a_id = RequestId::from("lower-select-project-a".to_string());
    lower_server.send_request(
        lower_select_a_id.clone(),
        "pascal/selectProject",
        json!({
            "textDocument": {"uri": uri(&app_main)},
            "projectUri": uri(&app.join("A.dproj"))
        }),
    );
    assert!(lower_server.response(&lower_select_a_id).error.is_none());
    let lower_a_id = RequestId::from("lower-project-a-definition".to_string());
    lower_server.send_request(
        lower_a_id.clone(),
        "textDocument/definition",
        navigation_params(&app_main, main_source, "Provider", 0),
    );
    let lower_a_locations = result_locations(lower_server.response(&lower_a_id));
    assert_eq!(lower_a_locations.len(), 1);
    assert_eq!(
        lower_a_locations[0]["uri"],
        uri(&workspace_provider).to_string()
    );

    let lower_select_b_id = RequestId::from("lower-select-project-b".to_string());
    lower_server.send_request(
        lower_select_b_id.clone(),
        "pascal/selectProject",
        json!({
            "textDocument": {"uri": uri(&app_main)},
            "projectUri": uri(&app.join("B.dproj"))
        }),
    );
    assert!(lower_server.response(&lower_select_b_id).error.is_none());
    let lower_b_id = RequestId::from("lower-project-b-definition".to_string());
    lower_server.send_request(
        lower_b_id.clone(),
        "textDocument/definition",
        navigation_params(&app_main, main_source, "Provider", 0),
    );
    let lower_b_locations = result_locations(lower_server.response(&lower_b_id));
    assert_eq!(lower_b_locations.len(), 1);
    assert_eq!(lower_b_locations[0]["uri"], uri(&user_provider).to_string());
    lower_server.shutdown();

    write_file(
        &app.join(".delphi-tools.local.toml"),
        &format!(
            "[[path_mappings]]\nfrom = 'C:\\SDK_A'\nto = '{}'\n[[path_mappings]]\nfrom = 'C:\\SDK_B'\nto = '{}'\n",
            project_sdk_a.path().display(),
            project_sdk_b.path().display()
        ),
    );

    let mut server = TestServer::launch_with_environment_path(environment.path());
    server.initialize(root.path(), Value::Null);

    let workspace_id = RequestId::from("workspace-precedence".to_string());
    server.send_request(
        workspace_id.clone(),
        "textDocument/definition",
        navigation_params(&workspace_main, main_source, "Provider", 0),
    );
    let workspace_locations = result_locations(server.response(&workspace_id));
    assert_eq!(workspace_locations.len(), 1);
    assert_eq!(
        workspace_locations[0]["uri"],
        uri(&workspace_provider).to_string()
    );

    let select_a_id = RequestId::from("select-project-a".to_string());
    server.send_request(
        select_a_id.clone(),
        "pascal/selectProject",
        json!({
            "textDocument": {"uri": uri(&app_main)},
            "projectUri": uri(&app.join("A.dproj"))
        }),
    );
    let selected_a = server.response(&select_a_id);
    assert!(selected_a.error.is_none(), "{selected_a:?}");
    assert_eq!(
        selected_a.result.unwrap()["selectedProjectUri"],
        uri(&app.join("A.dproj")).to_string()
    );

    let project_a_id = RequestId::from("project-a-definition".to_string());
    server.send_request(
        project_a_id.clone(),
        "textDocument/definition",
        navigation_params(&app_main, main_source, "Provider", 0),
    );
    let project_a_locations = result_locations(server.response(&project_a_id));
    assert_eq!(project_a_locations.len(), 1);
    assert_eq!(
        project_a_locations[0]["uri"],
        uri(&project_provider_a).to_string()
    );

    let select_b_id = RequestId::from("select-project-b".to_string());
    server.send_request(
        select_b_id.clone(),
        "pascal/selectProject",
        json!({
            "textDocument": {"uri": uri(&app_main)},
            "projectUri": uri(&app.join("B.dproj"))
        }),
    );
    let selected_b = server.response(&select_b_id);
    assert!(selected_b.error.is_none(), "{selected_b:?}");
    assert_eq!(
        selected_b.result.unwrap()["selectedProjectUri"],
        uri(&app.join("B.dproj")).to_string()
    );

    let project_b_id = RequestId::from("project-b-definition".to_string());
    server.send_request(
        project_b_id.clone(),
        "textDocument/definition",
        navigation_params(&app_main, main_source, "Provider", 0),
    );
    let project_b_locations = result_locations(server.response(&project_b_id));
    assert_eq!(project_b_locations.len(), 1);
    assert_eq!(
        project_b_locations[0]["uri"],
        uri(&project_provider_b).to_string()
    );

    let select_a_again_id = RequestId::from("select-project-a-again".to_string());
    server.send_request(
        select_a_again_id.clone(),
        "pascal/selectProject",
        json!({
            "textDocument": {"uri": uri(&app_main)},
            "projectUri": uri(&app.join("A.dproj"))
        }),
    );
    let selected_a_again = server.response(&select_a_again_id);
    assert!(selected_a_again.error.is_none(), "{selected_a_again:?}");

    let project_a_again_id = RequestId::from("project-a-definition-again".to_string());
    server.send_request(
        project_a_again_id.clone(),
        "textDocument/definition",
        navigation_params(&app_main, main_source, "Provider", 0),
    );
    let project_a_again_locations = result_locations(server.response(&project_a_again_id));
    assert_eq!(project_a_again_locations.len(), 1);
    assert_eq!(
        project_a_again_locations[0]["uri"],
        uri(&project_provider_a).to_string()
    );

    server.shutdown();
}

#[cfg(unix)]
#[test]
fn delphi_overrides_restart_keeps_the_captured_mapping_until_server_exit() {
    let environment = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let old_sdk = tempfile::tempdir().unwrap();
    let new_sdk = tempfile::tempdir().unwrap();
    let main = root.path().join("Main.pas");
    let project = root.path().join("Main.dproj");
    let old_provider = old_sdk.path().join("source/Provider.pas");
    let old_consumer = old_sdk.path().join("source/Consumer.pas");
    let new_provider = new_sdk.path().join("source/Provider.pas");
    let new_consumer = new_sdk.path().join("source/Consumer.pas");
    let config = environment.path().join("config/delphi-tools/config.toml");
    let main_source = "unit Main;\ninterface\nuses Provider;\nimplementation\nprocedure Run;\nbegin\n  Log(SharedValue);\nend;\nend.\n";
    let provider_source =
        "unit Provider;\ninterface\nconst SharedValue = 1;\nimplementation\nend.\n";
    let consumer_source = "unit Consumer;\ninterface\nuses Provider;\nimplementation\nprocedure Consume;\nbegin\n  Log(SharedValue);\nend;\nend.\n";
    write_file(&main, main_source);
    write_file(
        &project,
        "<Project><PropertyGroup><MainSource>Main.pas</MainSource><DCC_UnitSearchPath>$(BDS)\\source</DCC_UnitSearchPath></PropertyGroup></Project>",
    );
    for (provider, consumer) in [
        (&old_provider, &old_consumer),
        (&new_provider, &new_consumer),
    ] {
        write_file(provider, provider_source);
        write_file(consumer, consumer_source);
    }

    let mapping = |sdk: &Path| {
        format!(
            "[properties]\nBDS = 'C:\\SDK'\n[[path_mappings]]\nfrom = 'C:\\SDK'\nto = '{}'\n",
            sdk.display()
        )
    };
    write_file(&config, &mapping(old_sdk.path()));

    {
        let mut server = TestServer::launch_with_environment_path(environment.path());
        server.initialize(root.path(), Value::Null);
        server.send_notification(
            "textDocument/didOpen",
            json!({
                "textDocument": {
                    "uri": uri(&main),
                    "languageId": "pascal",
                    "version": 1,
                    "text": main_source
                }
            }),
        );

        let definition_id = RequestId::from("restart-old-definition".to_string());
        server.send_request(
            definition_id.clone(),
            "textDocument/definition",
            navigation_params(&main, main_source, "Provider", 0),
        );
        let definition = result_locations(server.response(&definition_id));
        assert_eq!(definition.len(), 1);
        assert_eq!(definition[0]["uri"], uri(&old_provider).to_string());

        let references_id = RequestId::from("restart-old-references".to_string());
        server.send_request(
            references_id.clone(),
            "textDocument/references",
            json!({
                "textDocument": {"uri": uri(&old_provider)},
                "position": position_of(provider_source, "SharedValue", 0),
                "context": {"includeDeclaration": false}
            }),
        );
        let references = result_locations(server.response(&references_id));
        let reference_uris = references
            .iter()
            .filter_map(|location| location["uri"].as_str())
            .collect::<HashSet<_>>();
        let old_consumer_uri = uri(&old_consumer).to_string();
        let new_consumer_uri = uri(&new_consumer).to_string();
        assert!(reference_uris.contains(old_consumer_uri.as_str()));
        assert!(!reference_uris.contains(new_consumer_uri.as_str()));

        write_file(&config, &mapping(new_sdk.path()));

        let definition_id = RequestId::from("restart-captured-definition".to_string());
        server.send_request(
            definition_id.clone(),
            "textDocument/definition",
            navigation_params(&main, main_source, "Provider", 0),
        );
        let definition = result_locations(server.response(&definition_id));
        assert_eq!(definition.len(), 1);
        assert_eq!(definition[0]["uri"], uri(&old_provider).to_string());

        let references_id = RequestId::from("restart-captured-references".to_string());
        server.send_request(
            references_id.clone(),
            "textDocument/references",
            json!({
                "textDocument": {"uri": uri(&old_provider)},
                "position": position_of(provider_source, "SharedValue", 0),
                "context": {"includeDeclaration": false}
            }),
        );
        let references = result_locations(server.response(&references_id));
        let reference_uris = references
            .iter()
            .filter_map(|location| location["uri"].as_str())
            .collect::<HashSet<_>>();
        let old_consumer_uri = uri(&old_consumer).to_string();
        let new_consumer_uri = uri(&new_consumer).to_string();
        assert!(reference_uris.contains(old_consumer_uri.as_str()));
        assert!(!reference_uris.contains(new_consumer_uri.as_str()));
        server.shutdown();
    }

    {
        let mut server = TestServer::launch_with_environment_path(environment.path());
        server.initialize(root.path(), Value::Null);
        server.send_notification(
            "textDocument/didOpen",
            json!({
                "textDocument": {
                    "uri": uri(&main),
                    "languageId": "pascal",
                    "version": 1,
                    "text": main_source
                }
            }),
        );

        let definition_id = RequestId::from("restart-new-definition".to_string());
        server.send_request(
            definition_id.clone(),
            "textDocument/definition",
            navigation_params(&main, main_source, "Provider", 0),
        );
        let definition = result_locations(server.response(&definition_id));
        assert_eq!(definition.len(), 1);
        assert_eq!(definition[0]["uri"], uri(&new_provider).to_string());

        let references_id = RequestId::from("restart-new-references".to_string());
        server.send_request(
            references_id.clone(),
            "textDocument/references",
            json!({
                "textDocument": {"uri": uri(&new_provider)},
                "position": position_of(provider_source, "SharedValue", 0),
                "context": {"includeDeclaration": false}
            }),
        );
        let references = result_locations(server.response(&references_id));
        let reference_uris = references
            .iter()
            .filter_map(|location| location["uri"].as_str())
            .collect::<HashSet<_>>();
        let old_consumer_uri = uri(&old_consumer).to_string();
        let new_consumer_uri = uri(&new_consumer).to_string();
        assert!(reference_uris.contains(new_consumer_uri.as_str()));
        assert!(!reference_uris.contains(old_consumer_uri.as_str()));
        server.shutdown();
    }

    assert_eq!(fs::read_to_string(&main).unwrap(), main_source);
}

#[test]
fn initialize_advertises_utf16_sync_navigation_and_formatting() {
    let (_temp, main, _provider, _main_source, _provider_source) = standard_workspace();
    let root = main.parent().expect("workspace root");
    let mut server = TestServer::launch();
    let result = server.initialize(root, Value::Null);
    let capabilities = &result["capabilities"];
    assert_eq!(capabilities["positionEncoding"], "utf-16");
    assert_eq!(capabilities["textDocumentSync"]["change"], 2);
    assert_eq!(capabilities["declarationProvider"], true);
    assert_eq!(capabilities["definitionProvider"], true);
    assert_eq!(capabilities["implementationProvider"], true);
    assert_eq!(capabilities["documentSymbolProvider"], true);
    assert_eq!(capabilities["workspaceSymbolProvider"], true);
    assert_eq!(capabilities["referencesProvider"], true);
    assert_eq!(capabilities["documentHighlightProvider"], true);
    assert_eq!(capabilities["selectionRangeProvider"], true);
    assert_eq!(capabilities["hoverProvider"], true);
    assert_eq!(capabilities["typeDefinitionProvider"], true);
    assert_eq!(capabilities["foldingRangeProvider"], true);
    assert_eq!(capabilities["documentFormattingProvider"], true);
    assert_eq!(capabilities["experimental"]["projectSelection"], true);
    server.shutdown();
}

#[test]
fn selection_ranges_return_an_inner_to_outer_structural_chain() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let source_path = temp.path().join("Selection.pas");
    let source = "unit Selection;\ninterface\ntype\n  TWidget = class\n    Value: Integer;\n  end;\nimplementation\nprocedure TWidget.Run;\nbegin\n  Value := Other.Bar[0] + 1;\nend;\nend.\n";
    write_file(&source_path, source);

    let mut server = TestServer::launch();
    let initialize = server.initialize(temp.path(), Value::Null);
    assert_eq!(initialize["capabilities"]["selectionRangeProvider"], true);

    let position = position_of(source, "Bar", 0);
    let id = RequestId::from("selection-ranges".to_string());
    server.send_request(
        id.clone(),
        "textDocument/selectionRange",
        json!({
            "textDocument": {"uri": uri(&source_path)},
            "positions": [position, position]
        }),
    );
    let response = server.response(&id);
    assert!(
        response.error.is_none(),
        "selection range request failed: {response:?}"
    );
    let result = response.result.expect("selection range result");
    let ranges = result.as_array().expect("selection range array");
    assert_eq!(
        ranges.len(),
        2,
        "request order and duplicates must be preserved"
    );
    assert_eq!(
        ranges[0], ranges[1],
        "duplicate positions must produce duplicate results"
    );
    assert_eq!(
        ranges[0]["range"],
        json!({
            "start": position,
            "end": Position::new(position.line, position.character + 3)
        }),
        "the innermost range must be the identifier under the cursor"
    );

    let mut chain = Vec::new();
    let mut current = &ranges[0];
    loop {
        chain.push(current["range"].clone());
        let Some(parent) = current.get("parent") else {
            break;
        };
        current = parent;
    }
    assert!(chain.len() >= 5, "structural chain is too short: {chain:?}");
    let point = |value: &Value| {
        (
            value["line"].as_u64().expect("selection line"),
            value["character"].as_u64().expect("selection character"),
        )
    };
    for pair in chain.windows(2) {
        let outer_start = point(&pair[1]["start"]);
        let inner_start = point(&pair[0]["start"]);
        let inner_end = point(&pair[0]["end"]);
        let outer_end = point(&pair[1]["end"]);
        assert!(
            outer_start <= inner_start && inner_end <= outer_end && pair[1] != pair[0],
            "selection parents must strictly contain their children: {chain:?}"
        );
    }
    assert_eq!(
        chain.last().expect("document fallback range"),
        &json!({
            "start": Position::new(0, 0),
            "end": pascal_lsp::text::offset_to_position(source, source.len())
                .expect("document end position")
        }),
        "the outermost selection must cover the document"
    );
    server.shutdown();
}

#[test]
fn selection_ranges_accept_crlf_line_comments_in_mixed_position_batches() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let source_path = temp.path().join("SelectionComment.pas");
    let source = "unit U;\r\n// hello\r\ninterface\r\nimplementation\r\nend.\r\n";
    write_file(&source_path, source);

    let mut server = TestServer::launch();
    server.initialize(temp.path(), Value::Null);
    let id = RequestId::from("selection-comment-crlf".to_string());
    server.send_request(
        id.clone(),
        "textDocument/selectionRange",
        json!({
            "textDocument": {"uri": uri(&source_path)},
            "positions": [
                {"line": 1, "character": 3},
                {"line": 1, "character": 8},
                {"line": 2, "character": 0}
            ]
        }),
    );
    let response = server.response(&id);
    assert!(
        response.error.is_none(),
        "CRLF comment selection failed: {response:?}"
    );
    let result = response.result.expect("selection result");
    let ranges = result.as_array().expect("selection array");
    assert_eq!(ranges.len(), 3);
    assert_eq!(
        ranges[0]["range"],
        json!({
            "start": {"line": 1, "character": 0},
            "end": {"line": 1, "character": 8}
        })
    );
    assert_eq!(
        ranges[1]["range"]["start"],
        json!({"line": 0, "character": 0})
    );
    assert_eq!(
        ranges[2]["range"]["start"],
        json!({"line": 2, "character": 0})
    );
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn selection_range_cancellation_returns_the_standard_request_canceled_error() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let source_path = temp.path().join("SelectionCancel.pas");
    let source = "unit SelectionCancel;\ninterface\nimplementation\nprocedure Run;\nbegin\n  Value := 1;\nend;\nend.\n";
    write_file(&source_path, source);

    let (mut server, barrier) = TestServer::launch_with_selection_barrier(temp);
    server.initialize(source_path.parent().expect("workspace root"), Value::Null);
    let id = RequestId::from("selection-cancelled".to_string());
    server.send_request(
        id.clone(),
        "textDocument/selectionRange",
        json!({
            "textDocument": {"uri": uri(&source_path)},
            "positions": [position_of(source, "Value", 0)]
        }),
    );
    barrier.wait_until_entered();
    server.send_notification("$/cancelRequest", json!({"id": "selection-cancelled"}));
    let error = server
        .response(&id)
        .error
        .expect("cancelled selection request must fail");
    assert_eq!(error.code, -32800);
    assert_eq!(error.message, "request cancelled");
    server.shutdown();
}

#[test]
fn selection_ranges_reject_invalid_positions_without_partial_results() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let source_path = temp.path().join("SelectionInvalid.pas");
    write_file(
        &source_path,
        "unit SelectionInvalid;\ninterface\nimplementation\nend.\n",
    );

    let mut server = TestServer::launch();
    server.initialize(temp.path(), Value::Null);

    let invalid_params_id = RequestId::from("selection-invalid-params".to_string());
    server.send_request(
        invalid_params_id.clone(),
        "textDocument/selectionRange",
        json!({}),
    );
    assert_eq!(
        server
            .response(&invalid_params_id)
            .error
            .expect("invalid selection parameters error")
            .code,
        -32602
    );

    let invalid_position_id = RequestId::from("selection-invalid-position".to_string());
    server.send_request(
        invalid_position_id.clone(),
        "textDocument/selectionRange",
        json!({
            "textDocument": {"uri": uri(&source_path)},
            "positions": [{"line": 0, "character": 10_000}]
        }),
    );
    let invalid_position_error = server
        .response(&invalid_position_id)
        .error
        .expect("invalid selection position error");
    assert_eq!(invalid_position_error.code, -32803);
    assert!(
        invalid_position_error
            .message
            .contains("valid UTF-16 source boundary")
    );

    let oversized_id = RequestId::from("selection-oversized".to_string());
    server.send_request(
        oversized_id.clone(),
        "textDocument/selectionRange",
        json!({
            "textDocument": {"uri": uri(&source_path)},
            "positions": vec![json!({"line": 0, "character": 0}); 257]
        }),
    );
    let oversized_error = server
        .response(&oversized_id)
        .error
        .expect("oversized selection request error");
    assert_eq!(oversized_error.code, -32803);
    assert!(oversized_error.message.contains("more than 256 positions"));
    server.shutdown();
}

#[test]
fn selection_ranges_return_a_valid_empty_source_range() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let source_path = temp.path().join("SelectionEmpty.pas");
    write_file(&source_path, "");

    let mut server = TestServer::launch();
    server.initialize(temp.path(), Value::Null);
    let id = RequestId::from("selection-empty".to_string());
    server.send_request(
        id.clone(),
        "textDocument/selectionRange",
        json!({
            "textDocument": {"uri": uri(&source_path)},
            "positions": [{"line": 0, "character": 0}]
        }),
    );
    let response = server.response(&id);
    assert!(
        response.error.is_none(),
        "empty selection request failed: {response:?}"
    );
    assert_eq!(
        response.result.expect("empty selection result"),
        json!([{
            "range": {
                "start": {"line": 0, "character": 0},
                "end": {"line": 0, "character": 0}
            }
        }])
    );
    server.shutdown();
}

#[test]
fn selection_ranges_are_syntax_only_and_do_not_require_imports() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let source_path = temp.path().join("SelectionUnresolvedImport.pas");
    let source = "unit SelectionUnresolvedImport;\ninterface\nuses MissingSelectionProvider;\nimplementation\nprocedure Run;\nbegin\n  MissingSelectionProvider.Value := 1;\nend;\nend.\n";
    write_file(&source_path, source);

    let mut server = TestServer::launch();
    server.initialize(temp.path(), Value::Null);
    let id = RequestId::from("selection-unresolved-import".to_string());
    server.send_request(
        id.clone(),
        "textDocument/selectionRange",
        json!({
            "textDocument": {"uri": uri(&source_path)},
            "positions": [position_of(source, "Value", 0)]
        }),
    );
    let response = server.response(&id);
    assert!(
        response.error.is_none(),
        "unresolved imports must not block syntax selection ranges: {response:?}"
    );
    assert_eq!(
        response
            .result
            .expect("selection result")
            .as_array()
            .expect("selection array")
            .len(),
        1
    );
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn blocked_selection_ranges_do_not_block_unrelated_lsp_requests() {
    let environment = tempfile::tempdir().expect("isolated server environment");
    let root = environment.path().join("workspace");
    let source_path = root.join("SelectionConcurrent.pas");
    let source = "unit SelectionConcurrent;\ninterface\nimplementation\nprocedure Run;\nbegin\n  Value := Other.Bar[0];\nend;\nend.\n";
    write_file(&source_path, source);

    let (mut server, barrier) = TestServer::launch_with_selection_barrier(environment);
    server.initialize(&root, Value::Null);
    let selection_id = RequestId::from("blocked-selection".to_string());
    server.send_request(
        selection_id.clone(),
        "textDocument/selectionRange",
        json!({
            "textDocument": {"uri": uri(&source_path)},
            "positions": [position_of(source, "Bar", 0)]
        }),
    );
    barrier.wait_until_entered();

    let symbols_id = RequestId::from("while-selection-is-blocked".to_string());
    server.send_request(
        symbols_id.clone(),
        "textDocument/documentSymbol",
        json!({"textDocument": {"uri": uri(&source_path)}}),
    );
    let symbols = server.response(&symbols_id);
    assert!(
        symbols.error.is_none() && symbols.result.is_some(),
        "unrelated request was blocked by selection ranges: {symbols:?}"
    );

    barrier.release();
    let selection = server.response(&selection_id);
    assert!(
        selection.error.is_none() && selection.result.is_some(),
        "selection range request failed after release: {selection:?}"
    );
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn requested_open_document_change_discards_blocked_selection_ranges() {
    let environment = tempfile::tempdir().expect("isolated server environment");
    let root = environment.path().join("workspace");
    let source_path = root.join("SelectionStale.pas");
    let first_source = "unit SelectionStale;\ninterface\nimplementation\nprocedure Run;\nbegin\n  Value := 1;\nend;\nend.\n";
    let second_source = first_source.replace("Value", "ChangedValue");
    write_file(&source_path, first_source);

    let (mut server, barrier) = TestServer::launch_with_selection_barrier(environment);
    server.initialize(&root, Value::Null);
    server.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri(&source_path),
                "languageId": "pascal",
                "version": 1,
                "text": first_source
            }
        }),
    );

    let request_id = RequestId::from("stale-selection".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/selectionRange",
        json!({
            "textDocument": {"uri": uri(&source_path)},
            "positions": [position_of(first_source, "Value", 0)]
        }),
    );
    barrier.wait_until_entered();
    server.send_notification(
        "textDocument/didChange",
        json!({
            "textDocument": {"uri": uri(&source_path), "version": 2},
            "contentChanges": [{"text": second_source}]
        }),
    );

    barrier.release();
    let response = server.response(&request_id);
    let error = response
        .error
        .expect("requested source change must stale selection ranges");
    assert_eq!(error.code, -32803);
    assert_eq!(
        error.message,
        "analysis result became stale; retry the request"
    );
    server.shutdown();
}

fn folding_ranges_request(
    server: &mut TestServer,
    request_name: &str,
    source_path: &Path,
) -> Vec<Value> {
    let id = RequestId::from(request_name.to_string());
    server.send_request(
        id.clone(),
        "textDocument/foldingRange",
        json!({"textDocument": {"uri": uri(source_path)}}),
    );
    let response = server.response(&id);
    assert!(
        response.error.is_none(),
        "folding range request failed: {response:?}"
    );
    response
        .result
        .expect("folding range result")
        .as_array()
        .expect("folding range array")
        .clone()
}

fn assert_folding_ranges_non_crossing(ranges: &[Value], include_characters: bool) {
    let point = |range: &Value, prefix: &str| {
        let line = range[format!("{prefix}Line")]
            .as_u64()
            .expect("folding range line");
        let character = include_characters
            .then(|| {
                range[format!("{prefix}Character")]
                    .as_u64()
                    .expect("character-mode folding range character")
            })
            .unwrap_or_default();
        (line, character)
    };

    for (left_index, left) in ranges.iter().enumerate() {
        let left_start = point(left, "start");
        let left_end = point(left, "end");
        for (right_index, right) in ranges.iter().enumerate().skip(left_index + 1) {
            let right_start = point(right, "start");
            let right_end = point(right, "end");
            let crossing =
                (left_start < right_start && right_start < left_end && left_end < right_end)
                    || (right_start < left_start && left_start < right_end && right_end < left_end);
            assert!(
                !crossing,
                "folding ranges {left_index} and {right_index} cross: {left:?} vs {right:?}"
            );
        }
    }
}

#[test]
fn folding_ranges_return_multiline_syntax_ranges_over_the_protocol() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let source_path = temp.path().join("Folding.pas");
    let source = "unit Folding;\ninterface\nprocedure Run;\nimplementation\nprocedure Run;\nbegin\n  if True then\n  begin\n    Value := 1;\n  end;\nend;\nend.\n";
    write_file(&source_path, source);

    let mut server = TestServer::launch();
    let initialize = server.initialize(temp.path(), Value::Null);
    assert_eq!(initialize["capabilities"]["foldingRangeProvider"], true);

    let id = RequestId::from("folding-ranges".to_string());
    server.send_request(
        id.clone(),
        "textDocument/foldingRange",
        json!({"textDocument": {"uri": uri(&source_path)}}),
    );
    let response = server.response(&id);
    assert!(
        response.error.is_none(),
        "folding range request failed: {response:?}"
    );
    let result = response.result.expect("folding range result");
    let ranges = result.as_array().expect("folding range array");
    assert!(
        ranges
            .iter()
            .any(|range| { range["startLine"] == 4 && range["endLine"] == 10 }),
        "routine range missing from response: {ranges:?}"
    );
    assert!(
        ranges
            .iter()
            .any(|range| { range["startLine"] == 5 && range["endLine"] == 10 }),
        "begin range missing from response: {ranges:?}"
    );
    assert!(
        ranges
            .iter()
            .any(|range| { range["startLine"] == 6 && range["endLine"] == 9 }),
        "if range missing from response: {ranges:?}"
    );
    server.shutdown();
}

#[test]
fn folding_ranges_reconcile_crossing_implementation_and_region_candidates() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let source_path = temp.path().join("FoldingCrossing.pas");
    let source = "unit X;\ninterface\nimplementation\n{$REGION R}\nprocedure P;\nbegin\nend;\n{$ENDREGION}\nend.\n";
    write_file(&source_path, source);

    let mut character_server = TestServer::launch();
    character_server.initialize(temp.path(), Value::Null);
    let character_ranges = folding_ranges_request(
        &mut character_server,
        "folding-crossing-characters",
        &source_path,
    );
    assert_folding_ranges_non_crossing(&character_ranges, true);
    assert!(
        character_ranges.iter().any(|range| {
            range["kind"] == "region"
                && range["startLine"] == 3
                && range["startCharacter"] == 0
                && range["endLine"] == 7
                && range["endCharacter"] == 12
        }),
        "balanced region range must be preserved: {character_ranges:?}"
    );
    assert!(
        character_ranges.iter().any(|range| {
            range["startLine"] == 4
                && range["startCharacter"] == 0
                && range["endLine"] == 6
                && range["endCharacter"] == 4
        }),
        "routine range nested inside the region must be preserved: {character_ranges:?}"
    );
    assert!(
        character_ranges.iter().any(|range| {
            range["startLine"] == 2
                && range["startCharacter"] == 0
                && range["endLine"] == 7
                && range["endCharacter"] == 12
        }),
        "implementation section must be extended over trailing region trivia: {character_ranges:?}"
    );
    character_server.shutdown();

    let mut line_only_server = TestServer::launch();
    line_only_server
        .initialize_with_folding_capabilities(temp.path(), json!({"lineFoldingOnly": true}));
    let line_only_ranges = folding_ranges_request(
        &mut line_only_server,
        "folding-crossing-line-only",
        &source_path,
    );
    assert_folding_ranges_non_crossing(&line_only_ranges, false);
    assert!(
        line_only_ranges.iter().all(|range| {
            !range.as_object().is_some_and(|range| {
                range.contains_key("startCharacter") || range.contains_key("endCharacter")
            })
        }),
        "line-only folding ranges must omit character fields: {line_only_ranges:?}"
    );
    assert!(
        line_only_ranges.iter().any(|range| {
            range["kind"] == "region" && range["startLine"] == 3 && range["endLine"] == 7
        }),
        "line-only region range must be preserved: {line_only_ranges:?}"
    );
    assert!(
        line_only_ranges
            .iter()
            .any(|range| { range["startLine"] == 2 && range["endLine"] == 7 }),
        "line-only implementation section must include the trailing region directive: {line_only_ranges:?}"
    );
    line_only_server.shutdown();

    let mut zero_limit_server = TestServer::launch();
    zero_limit_server.initialize_with_folding_capabilities(temp.path(), json!({"rangeLimit": 0}));
    let zero_limit_ranges = folding_ranges_request(
        &mut zero_limit_server,
        "folding-crossing-zero-limit",
        &source_path,
    );
    assert!(
        zero_limit_ranges.is_empty(),
        "zero range limit must remain empty"
    );
    zero_limit_server.shutdown();

    let mut small_limit_server = TestServer::launch();
    small_limit_server.initialize_with_folding_capabilities(temp.path(), json!({"rangeLimit": 2}));
    let small_limit_ranges = folding_ranges_request(
        &mut small_limit_server,
        "folding-crossing-small-limit",
        &source_path,
    );
    assert!(
        small_limit_ranges.len() <= 2,
        "small range limit must be honored: {small_limit_ranges:?}"
    );
    assert_folding_ranges_non_crossing(&small_limit_ranges, true);
    small_limit_server.shutdown();
}

#[test]
fn folding_range_client_capabilities_filter_kinds_and_characters() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let source_path = temp.path().join("FoldingOptions.pas");
    let source = "unit FoldingOptions;\ninterface\nimplementation\n{$REGION 'body'}\n{comment\n  continues}\nprocedure Run;\nbegin\n  Value := 1;\nend;\n{$ENDREGION}\nend.\n";
    write_file(&source_path, source);

    let mut server = TestServer::launch();
    server.initialize_with_folding_capabilities(
        temp.path(),
        json!({
            "lineFoldingOnly": true,
            "foldingRangeKind": {"valueSet": ["region"]}
        }),
    );
    let id = RequestId::from("folding-options".to_string());
    server.send_request(
        id.clone(),
        "textDocument/foldingRange",
        json!({"textDocument": {"uri": uri(&source_path)}}),
    );
    let response = server.response(&id);
    assert!(
        response.error.is_none(),
        "folding range request failed: {response:?}"
    );
    let result = response.result.expect("folding range result");
    let ranges = result.as_array().expect("folding range array");
    assert!(
        ranges.iter().any(|range| range["kind"] == "region"),
        "region range missing: {ranges:?}"
    );
    assert!(
        ranges
            .iter()
            .all(|range| range.get("kind") != Some(&json!("comment"))),
        "unsupported comment range returned: {ranges:?}"
    );
    assert!(
        ranges.iter().all(|range| {
            !range.as_object().is_some_and(|range| {
                range.contains_key("startCharacter") || range.contains_key("endCharacter")
            })
        }),
        "line-only ranges must omit character fields: {ranges:?}"
    );
    server.shutdown();
}

#[test]
fn folding_range_zero_limit_returns_an_empty_result() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let source_path = temp.path().join("FoldingZeroLimit.pas");
    let source =
        "unit FoldingZeroLimit;\ninterface\nimplementation\nprocedure Run;\nbegin\nend;\nend.\n";
    write_file(&source_path, source);

    let mut server = TestServer::launch();
    server.initialize_with_folding_capabilities(temp.path(), json!({"rangeLimit": 0}));
    let id = RequestId::from("folding-zero-limit".to_string());
    server.send_request(
        id.clone(),
        "textDocument/foldingRange",
        json!({"textDocument": {"uri": uri(&source_path)}}),
    );
    let response = server.response(&id);
    assert!(
        response.error.is_none(),
        "folding range request failed: {response:?}"
    );
    assert_eq!(response.result.expect("folding range result"), json!([]));
    server.shutdown();
}

#[test]
fn folding_range_limit_prefers_outer_meaningful_ranges() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let source_path = temp.path().join("FoldingLimit.pas");
    let source = "unit FoldingLimit;\ninterface\nprocedure Decl;\nimplementation\nprocedure Run;\nbegin\n  if True then\n  begin\n  end;\nend;\nend.\n";
    write_file(&source_path, source);

    let mut server = TestServer::launch();
    server.initialize_with_folding_capabilities(temp.path(), json!({"rangeLimit": 2}));
    let id = RequestId::from("folding-limit".to_string());
    server.send_request(
        id.clone(),
        "textDocument/foldingRange",
        json!({"textDocument": {"uri": uri(&source_path)}}),
    );
    let response = server.response(&id);
    assert!(
        response.error.is_none(),
        "folding range request failed: {response:?}"
    );
    let result = response.result.expect("folding range result");
    let ranges = result.as_array().expect("folding range array");
    assert_eq!(ranges.len(), 2, "range limit must be honored: {ranges:?}");
    assert_eq!(
        ranges
            .iter()
            .map(|range| (range["startLine"].as_u64(), range["endLine"].as_u64()))
            .collect::<Vec<_>>(),
        vec![(Some(3), Some(9)), (Some(4), Some(9))]
    );
    server.shutdown();
}

#[test]
fn line_only_folding_does_not_hide_code_after_a_closing_token() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let source_path = temp.path().join("FoldingLineOnly.pas");
    let source = "unit FoldingLineOnly;\ninterface\nimplementation\nprocedure Run;\nbegin\n  if True then begin\n    Value := 1;\n  end; Value := 2;\nend;\nend.\n";
    write_file(&source_path, source);

    let mut server = TestServer::launch();
    server.initialize_with_folding_capabilities(temp.path(), json!({"lineFoldingOnly": true}));
    let id = RequestId::from("folding-line-only".to_string());
    server.send_request(
        id.clone(),
        "textDocument/foldingRange",
        json!({"textDocument": {"uri": uri(&source_path)}}),
    );
    let response = server.response(&id);
    assert!(
        response.error.is_none(),
        "folding range request failed: {response:?}"
    );
    let result = response.result.expect("folding range result");
    let ranges = result.as_array().expect("folding range array");
    assert!(
        ranges
            .iter()
            .any(|range| { range["startLine"] == 5 && range["endLine"] == 6 }),
        "if body should stop before the line containing unrelated code: {ranges:?}"
    );
    assert!(
        ranges
            .iter()
            .all(|range| { !(range["startLine"] == 5 && range["endLine"] == 7) }),
        "line-only folding must not hide unrelated closing-line code: {ranges:?}"
    );
    server.shutdown();
}

#[test]
fn folding_ranges_use_authoritative_open_overlay_then_restore_disk_source() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let source_path = temp.path().join("FoldingOverlay.pas");
    let disk_source =
        "unit FoldingOverlay;\ninterface\nimplementation\nprocedure Run; begin end;\nend.\n";
    let overlay_source = "unit FoldingOverlay;\ninterface\nimplementation\nprocedure Run;\nbegin\n  Value := 1;\nend;\nend.\n";
    write_file(&source_path, disk_source);

    let mut server = TestServer::launch();
    server.initialize(temp.path(), Value::Null);
    server.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri(&source_path),
                "languageId": "pascal",
                "version": 1,
                "text": overlay_source
            }
        }),
    );

    let open_id = RequestId::from("folding-overlay-open".to_string());
    server.send_request(
        open_id.clone(),
        "textDocument/foldingRange",
        json!({"textDocument": {"uri": uri(&source_path)}}),
    );
    let open_response = server.response(&open_id);
    assert!(
        open_response.error.is_none(),
        "open overlay request failed: {open_response:?}"
    );
    let open_result = open_response.result.expect("open folding result");
    let open_ranges = open_result.as_array().expect("open folding array");
    assert!(
        open_ranges
            .iter()
            .any(|range| { range["startLine"] == 3 && range["endLine"] == 6 }),
        "folding must use the open overlay: {open_ranges:?}"
    );

    server.send_notification(
        "textDocument/didClose",
        json!({"textDocument": {"uri": uri(&source_path)}}),
    );
    let close_id = RequestId::from("folding-overlay-close".to_string());
    server.send_request(
        close_id.clone(),
        "textDocument/foldingRange",
        json!({"textDocument": {"uri": uri(&source_path)}}),
    );
    let close_response = server.response(&close_id);
    assert!(
        close_response.error.is_none(),
        "closed overlay request failed: {close_response:?}"
    );
    let close_result = close_response.result.expect("closed folding result");
    let close_ranges = close_result.as_array().expect("closed folding array");
    assert!(
        close_ranges
            .iter()
            .all(|range| { !(range["startLine"] == 3 && range["endLine"] == 6) }),
        "folding must return to disk source after close: {close_ranges:?}"
    );
    server.shutdown();
}

#[test]
fn folding_ranges_do_not_require_imports_to_resolve() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let source_path = temp.path().join("FoldingMissingImport.pas");
    let source = "unit FoldingMissingImport;\ninterface\nuses MissingProvider;\nimplementation\nprocedure Run;\nbegin\nend;\nend.\n";
    write_file(&source_path, source);

    let mut server = TestServer::launch();
    server.initialize(temp.path(), Value::Null);
    let id = RequestId::from("folding-missing-import".to_string());
    server.send_request(
        id.clone(),
        "textDocument/foldingRange",
        json!({"textDocument": {"uri": uri(&source_path)}}),
    );
    let response = server.response(&id);
    assert!(
        response.error.is_none(),
        "missing import must not block folding: {response:?}"
    );
    let result = response.result.expect("folding range result");
    let ranges = result.as_array().expect("folding range array");
    assert!(
        ranges
            .iter()
            .any(|range| { range["startLine"] == 4 && range["endLine"] == 6 }),
        "routine range missing when an import is unresolved: {ranges:?}"
    );
    server.shutdown();
}

#[test]
fn folding_ranges_reject_invalid_and_non_file_documents() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let source_path = temp.path().join("FoldingInvalid.pas");
    write_file(
        &source_path,
        "unit FoldingInvalid;\ninterface\nimplementation\nend.\n",
    );

    let mut server = TestServer::launch();
    server.initialize(temp.path(), Value::Null);

    let invalid_id = RequestId::from("folding-invalid-params".to_string());
    server.send_request(invalid_id.clone(), "textDocument/foldingRange", json!({}));
    assert_eq!(
        server
            .response(&invalid_id)
            .error
            .expect("invalid folding parameters error")
            .code,
        -32602
    );

    let non_file_id = RequestId::from("folding-non-file".to_string());
    server.send_request(
        non_file_id.clone(),
        "textDocument/foldingRange",
        json!({"textDocument": {"uri": "https://example.test/Folding.pas"}}),
    );
    let non_file_error = server
        .response(&non_file_id)
        .error
        .expect("non-file folding error");
    assert_eq!(non_file_error.code, -32803);
    server.shutdown();
}

#[test]
fn folding_range_cancellation_handles_thousands_of_nodes() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let source_path = temp.path().join("FoldingMany.pas");
    let mut source = String::from("unit FoldingMany;\ninterface\nvar\n");
    for index in 0..4_000 {
        source.push_str(&format!("  Value{index}: Integer;\n"));
    }
    source.push_str("implementation\nend.\n");
    write_file(&source_path, &source);

    let mut server = TestServer::launch();
    server.initialize(temp.path(), Value::Null);
    let id = RequestId::from("folding-cancelled".to_string());
    server.send_request(
        id.clone(),
        "textDocument/foldingRange",
        json!({"textDocument": {"uri": uri(&source_path)}}),
    );
    server.send_notification("$/cancelRequest", json!({"id": "folding-cancelled"}));
    let error = server
        .response(&id)
        .error
        .expect("cancelled folding request must fail");
    assert_eq!(error.code, -32800);
    assert_eq!(error.message, "request cancelled");
    server.shutdown();
}

#[test]
fn incremental_did_change_updates_overlay_for_navigation() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let source_path = temp.path().join("Main.pas");
    let source = "unit Main;\ninterface\nprocedure OldRoutine;\nimplementation\nprocedure OldRoutine;\nprocedure Use;\nbegin\n  OldRoutine;\nend;\nend.\n";
    let updated = source.replace("OldRoutine", "NewLongRoutine");
    let after_first_edit = source.replacen("OldRoutine", "NewLongRoutine", 1);
    let after_second_edit = after_first_edit.replacen("OldRoutine", "NewLongRoutine", 1);
    write_file(&source_path, source);

    let first_start = position_of(source, "OldRoutine", 0);
    let first_end = position_after(source, "OldRoutine", 0);
    let second_start = position_of(&after_first_edit, "OldRoutine", 0);
    let second_end = position_after(&after_first_edit, "OldRoutine", 0);
    let third_start = position_of(&after_second_edit, "OldRoutine", 0);
    let third_end = position_after(&after_second_edit, "OldRoutine", 0);

    let mut server = TestServer::launch();
    server.initialize(temp.path(), Value::Null);
    server.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri(&source_path),
                "languageId": "pascal",
                "version": 1,
                "text": source
            }
        }),
    );
    server.send_notification(
        "textDocument/didChange",
        json!({
            "textDocument": {"uri": uri(&source_path), "version": 2},
            "contentChanges": [
                {"range": {"start": first_start, "end": first_end}, "text": "NewLongRoutine"},
                {"range": {"start": second_start, "end": second_end}, "text": "NewLongRoutine"},
                {"range": {"start": third_start, "end": third_end}, "text": "NewLongRoutine"}
            ]
        }),
    );

    let id = RequestId::from("incremental-definition".to_string());
    server.send_request(
        id.clone(),
        "textDocument/definition",
        navigation_params(&source_path, &updated, "NewLongRoutine", 2),
    );
    let locations = result_locations(server.response(&id));
    assert!(
        locations.iter().any(|location| {
            location["uri"] == uri(&source_path).to_string()
                && location["range"]
                    == json!({
                        "start": position_of(&updated, "NewLongRoutine", 0),
                        "end": position_after(&updated, "NewLongRoutine", 0)
                    })
        }),
        "definition should use the incrementally updated declaration: {locations:?}"
    );
    server.shutdown();
}

#[test]
fn malformed_did_change_position_desynchronizes_until_full_resynchronization() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let source_path = temp.path().join("Main.pas");
    let source = "unit Main;\ninterface\nprocedure OldRoutine;\nimplementation\nprocedure OldRoutine;\nprocedure Use;\nbegin\n  OldRoutine;\nend;\nend.\n";
    let updated = source.replace("OldRoutine", "NewRoutine");
    write_file(&source_path, source);

    let mut server = TestServer::launch();
    server.initialize(temp.path(), Value::Null);
    server.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri(&source_path),
                "languageId": "pascal",
                "version": 1,
                "text": source
            }
        }),
    );
    server.send_notification(
        "textDocument/didChange",
        json!({
            "textDocument": {"uri": uri(&source_path), "version": 2},
            "contentChanges": [{
                "range": {
                    "start": {"line": -1, "character": 0},
                    "end": {"line": -1, "character": 0}
                },
                "text": "NewRoutine"
            }]
        }),
    );

    let first_start = position_of(source, "OldRoutine", 0);
    let first_end = position_after(source, "OldRoutine", 0);
    server.send_notification(
        "textDocument/didChange",
        json!({
            "textDocument": {"uri": uri(&source_path), "version": 3},
            "contentChanges": [{
                "range": {"start": first_start, "end": first_end},
                "text": "NewRoutine"
            }]
        }),
    );
    let request_id = RequestId::from("malformed-position-ranged-change".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/definition",
        navigation_params(
            &source_path,
            &source.replacen("OldRoutine", "NewRoutine", 1),
            "NewRoutine",
            0,
        ),
    );
    assert!(
        result_locations(server.response(&request_id)).is_empty(),
        "a ranged change after malformed didChange must not use stale text"
    );

    server.send_notification(
        "textDocument/didChange",
        json!({
            "textDocument": {"uri": uri(&source_path), "version": 4},
            "contentChanges": [{"text": updated}]
        }),
    );
    let request_id = RequestId::from("malformed-position-full-resync".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/definition",
        navigation_params(&source_path, &updated, "NewRoutine", 2),
    );
    assert!(!result_locations(server.response(&request_id)).is_empty());
    server.shutdown();
}

#[test]
fn malformed_did_change_range_length_desynchronizes_until_close_and_reopen() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let source_path = temp.path().join("Main.pas");
    let source = "unit Main;\ninterface\nprocedure OldRoutine;\nimplementation\nprocedure OldRoutine;\nprocedure Use;\nbegin\n  OldRoutine;\nend;\nend.\n";
    write_file(&source_path, source);

    let mut server = TestServer::launch();
    server.initialize(temp.path(), Value::Null);
    server.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri(&source_path),
                "languageId": "pascal",
                "version": 1,
                "text": source
            }
        }),
    );
    let start = position_of(source, "OldRoutine", 0);
    let end = position_after(source, "OldRoutine", 0);
    server.send_notification(
        "textDocument/didChange",
        json!({
            "textDocument": {"uri": uri(&source_path), "version": 2},
            "contentChanges": [{
                "range": {"start": start, "end": end},
                "rangeLength": "11",
                "text": "NewRoutine"
            }]
        }),
    );

    server.send_notification(
        "textDocument/didChange",
        json!({
            "textDocument": {"uri": uri(&source_path), "version": 3},
            "contentChanges": [{
                "range": {"start": start, "end": end},
                "text": "NewRoutine"
            }]
        }),
    );
    let request_id = RequestId::from("malformed-range-length-ranged-change".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/definition",
        navigation_params(
            &source_path,
            &source.replacen("OldRoutine", "NewRoutine", 1),
            "NewRoutine",
            0,
        ),
    );
    assert!(
        result_locations(server.response(&request_id)).is_empty(),
        "a ranged change after malformed rangeLength must not use stale text"
    );

    server.send_notification(
        "textDocument/didClose",
        json!({"textDocument": {"uri": uri(&source_path)}}),
    );
    server.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri(&source_path),
                "languageId": "pascal",
                "version": 1,
                "text": source
            }
        }),
    );
    server.send_notification(
        "textDocument/didChange",
        json!({
            "textDocument": {"uri": uri(&source_path), "version": 2},
            "contentChanges": [{
                "range": {"start": start, "end": end},
                "text": "NewRoutine"
            }]
        }),
    );
    let request_id = RequestId::from("malformed-range-length-close-reopen".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/definition",
        navigation_params(
            &source_path,
            &source.replacen("OldRoutine", "NewRoutine", 1),
            "NewRoutine",
            0,
        ),
    );
    assert_eq!(result_locations(server.response(&request_id)).len(), 1);
    server.shutdown();
}

#[test]
fn malformed_did_change_without_trusted_newer_open_attribution_does_not_desynchronize() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let source_path = temp.path().join("Main.pas");
    let unknown_path = temp.path().join("Unknown.pas");
    let source = "unit Main;\ninterface\nprocedure OldRoutine;\nimplementation\nprocedure OldRoutine;\nprocedure Use;\nbegin\n  OldRoutine;\nend;\nend.\n";
    let updated = source.replacen("OldRoutine", "NewRoutine", 1);
    write_file(&source_path, source);

    let mut server = TestServer::launch();
    server.initialize(temp.path(), Value::Null);
    server.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri(&source_path),
                "languageId": "pascal",
                "version": 1,
                "text": source
            }
        }),
    );
    for params in [
        json!({
            "textDocument": {"uri": uri(&unknown_path), "version": 2},
            "contentChanges": [{
                "range": {
                    "start": {"line": -1, "character": 0},
                    "end": {"line": -1, "character": 0}
                },
                "text": "ignored"
            }]
        }),
        json!({
            "textDocument": {"uri": uri(&source_path)},
            "contentChanges": [{
                "range": {"start": {"line": -1, "character": 0}, "end": {"line": -1, "character": 0}},
                "text": "ignored"
            }]
        }),
        json!({
            "textDocument": {"uri": uri(&source_path), "version": 1},
            "contentChanges": [{
                "range": {"start": {"line": -1, "character": 0}, "end": {"line": -1, "character": 0}},
                "text": "ignored"
            }]
        }),
    ] {
        server.send_notification("textDocument/didChange", params);
    }

    let start = position_of(source, "OldRoutine", 0);
    let end = position_after(source, "OldRoutine", 0);
    server.send_notification(
        "textDocument/didChange",
        json!({
            "textDocument": {"uri": uri(&source_path), "version": 2},
            "contentChanges": [{
                "range": {"start": start, "end": end},
                "text": "NewRoutine"
            }]
        }),
    );
    let request_id = RequestId::from("untrusted-malformed-attribution".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/definition",
        navigation_params(&source_path, &updated, "NewRoutine", 0),
    );
    assert_eq!(result_locations(server.response(&request_id)).len(), 1);
    server.shutdown();
}

#[test]
fn incremental_did_change_applies_a_cross_line_crlf_range() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let source_path = temp.path().join("Main.pas");
    let source = "unit Main;\r\ninterface\r\nprocedure OldRoutine;\r\nimplementation\r\nend.\r\n";
    let start = position_of(source, "interface", 0);
    let end = position_after(source, "OldRoutine", 0);
    write_file(&source_path, source);

    let mut server = TestServer::launch();
    server.initialize(temp.path(), Value::Null);
    server.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri(&source_path),
                "languageId": "pascal",
                "version": 1,
                "text": source
            }
        }),
    );
    server.send_notification(
        "textDocument/didChange",
        json!({
            "textDocument": {"uri": uri(&source_path), "version": 2},
            "contentChanges": [{
                "range": {"start": start, "end": end},
                "text": "interface\r\nprocedure NewRoutine"
            }]
        }),
    );

    let id = RequestId::from("cross-line-crlf-symbols".to_string());
    server.send_request(
        id.clone(),
        "textDocument/documentSymbol",
        json!({"textDocument": {"uri": uri(&source_path)}}),
    );
    let response = server.response(&id);
    assert!(
        response.error.is_none(),
        "cross-line CRLF symbols failed: {response:?}"
    );
    let result = response.result.expect("cross-line CRLF symbols result");
    let symbols = result.as_array().expect("cross-line CRLF symbols array");
    assert!(
        symbols.iter().any(|symbol| symbol["name"] == "NewRoutine"),
        "incremental CRLF edit was not applied: {symbols:?}"
    );
    assert!(
        symbols.iter().all(|symbol| symbol["name"] != "OldRoutine"),
        "stale CRLF symbol remained after incremental edit: {symbols:?}"
    );
    assert_eq!(
        fs::read_to_string(&source_path).expect("source after edit"),
        source
    );
    server.shutdown();
}

#[test]
fn semantic_tokens_advertise_the_legend_and_return_full_document_tokens() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let source_path = temp.path().join("Tokens.pas");
    let source = "unit Tokens;\r\ninterface\r\ntype\r\n  TWidget = class\r\n    Value: Integer;\r\n  end;\r\nprocedure Run(A: Integer);\r\nimplementation\r\nprocedure Run(A: Integer);\r\nvar\r\n  Widget: TWidget;\r\nbegin\r\n  Widget.Value := 42; // 😀 value\r\n  WriteLn('hello');\r\nend;\r\nend.\r\n";
    write_file(&source_path, source);

    let mut server = TestServer::launch();
    let initialize = server.initialize(temp.path(), Value::Null);
    let provider = &initialize["capabilities"]["semanticTokensProvider"];
    assert_eq!(provider["range"], true);
    assert_eq!(provider["full"], true);
    assert_eq!(provider["full"].get("delta"), None);
    assert_eq!(
        provider["legend"]["tokenTypes"],
        json!([
            "namespace",
            "type",
            "class",
            "enum",
            "interface",
            "struct",
            "typeParameter",
            "parameter",
            "variable",
            "property",
            "enumMember",
            "function",
            "method",
            "keyword",
            "modifier",
            "comment",
            "string",
            "number",
            "operator"
        ])
    );
    assert_eq!(
        provider["legend"]["tokenModifiers"],
        json!(["declaration", "definition", "readonly", "static"])
    );

    let request_id = RequestId::from("semantic-tokens-full".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/semanticTokens/full",
        json!({"textDocument": {"uri": uri(&source_path)}}),
    );
    let response = server.response(&request_id);
    assert!(
        response.error.is_none(),
        "semantic tokens failed: {response:?}"
    );
    let result = response.result.expect("semantic token result");
    let data = result["data"].as_array().expect("semantic token data");
    assert!(!data.is_empty());
    assert_eq!(data.len() % 5, 0);
    let token_types = provider["legend"]["tokenTypes"]
        .as_array()
        .expect("token type legend")
        .iter()
        .map(|token_type| token_type.as_str().expect("token type name"))
        .collect::<Vec<_>>();
    let tokens = decoded_semantic_tokens(&result, &token_types);
    assert!(tokens.contains(&(0, 0, 4, "keyword".to_string(), 0)));
    assert!(tokens.contains(&(0, 5, 6, "namespace".to_string(), 1)));
    assert!(tokens.contains(&(12, 22, 11, "comment".to_string(), 0)));
    assert!(tokens.contains(&(12, 15, 2, "operator".to_string(), 0)));
    assert!(tokens.contains(&(12, 18, 2, "number".to_string(), 0)));
    assert!(tokens.contains(&(4, 4, 5, "variable".to_string(), 1)));
    assert!(tokens.contains(&(6, 10, 3, "function".to_string(), 1)));
    assert!(tokens.contains(&(8, 10, 3, "function".to_string(), 2)));

    server.shutdown();
}

#[test]
fn semantic_tokens_range_clips_tokens_to_the_requested_utf16_range() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let source_path = temp.path().join("RangeTokens.pas");
    let source = "unit RangeTokens;\ninterface\nprocedure Run;\nimplementation\nprocedure Run;\nvar\n  Value: Integer;\nbegin\n  Value := 42;\nend;\nend.\n";
    write_file(&source_path, source);

    let mut server = TestServer::launch();
    let initialize = server.initialize(temp.path(), Value::Null);
    let token_types = initialize["capabilities"]["semanticTokensProvider"]["legend"]["tokenTypes"]
        .as_array()
        .expect("token type legend")
        .iter()
        .map(|token_type| token_type.as_str().expect("token type name"))
        .collect::<Vec<_>>();
    let request_id = RequestId::from("semantic-tokens-range".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/semanticTokens/range",
        json!({
            "textDocument": {"uri": uri(&source_path)},
            "range": {
                "start": {"line": 8, "character": 3},
                "end": {"line": 8, "character": 7}
            }
        }),
    );
    let response = server.response(&request_id);
    assert!(
        response.error.is_none(),
        "semantic token range failed: {response:?}"
    );
    let result = response.result.expect("semantic token range result");
    assert_eq!(
        decoded_semantic_tokens(&result, &token_types),
        vec![(8, 3, 4, "variable".to_string(), 0)]
    );
    server.shutdown();
}

#[test]
fn semantic_tokens_full_request_uses_the_newest_open_document_overlay() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let source_path = temp.path().join("OverlayTokens.pas");
    let disk_source = "unit OverlayTokens;\ninterface\nprocedure Run;\nimplementation\nprocedure Run;\nvar\n  DiskValue: Integer;\nbegin\n  DiskValue := 1;\nend;\nend.\n";
    let overlay_source = disk_source.replace("DiskValue", "UpdatedValue");
    write_file(&source_path, disk_source);

    let mut server = TestServer::launch();
    let initialize = server.initialize(temp.path(), Value::Null);
    let token_types = initialize["capabilities"]["semanticTokensProvider"]["legend"]["tokenTypes"]
        .as_array()
        .expect("token type legend")
        .iter()
        .map(|token_type| token_type.as_str().expect("token type name"))
        .collect::<Vec<_>>();
    server.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri(&source_path),
                "languageId": "pascal",
                "version": 1,
                "text": disk_source
            }
        }),
    );
    server.send_notification(
        "textDocument/didChange",
        json!({
            "textDocument": {"uri": uri(&source_path), "version": 2},
            "contentChanges": [{"text": overlay_source}]
        }),
    );
    let request_id = RequestId::from("semantic-tokens-overlay".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/semanticTokens/full",
        json!({"textDocument": {"uri": uri(&source_path)}}),
    );
    let response = server.response(&request_id);
    assert!(
        response.error.is_none(),
        "overlay semantic tokens failed: {response:?}"
    );
    let result = response.result.expect("overlay semantic token result");
    let tokens = decoded_semantic_tokens(&result, &token_types);
    assert!(tokens.contains(&(6, 2, 12, "variable".to_string(), 1)));
    assert!(tokens.contains(&(8, 2, 12, "variable".to_string(), 0)));
    server.shutdown();
}

#[test]
fn semantic_tokens_mark_class_members_with_the_static_modifier() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let source_path = temp.path().join("StaticTokens.pas");
    let source = "unit StaticTokens;\ninterface\ntype\n  TWidget = class\n    class var Count: Integer;\n    class procedure Reset;\n  end;\nimplementation\nclass procedure TWidget.Reset;\nbegin\n  Count := 0;\nend;\nend.\n";
    write_file(&source_path, source);

    let mut server = TestServer::launch();
    let initialize = server.initialize(temp.path(), Value::Null);
    let token_types = initialize["capabilities"]["semanticTokensProvider"]["legend"]["tokenTypes"]
        .as_array()
        .expect("token type legend")
        .iter()
        .map(|token_type| token_type.as_str().expect("token type name"))
        .collect::<Vec<_>>();
    let request_id = RequestId::from("semantic-tokens-static".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/semanticTokens/full",
        json!({"textDocument": {"uri": uri(&source_path)}}),
    );
    let response = server.response(&request_id);
    assert!(
        response.error.is_none(),
        "semantic tokens failed: {response:?}"
    );
    let result = response.result.expect("semantic token result");
    let tokens = decoded_semantic_tokens(&result, &token_types);

    assert!(tokens.contains(&(4, 14, 5, "variable".to_string(), 1 | (1 << 3))));
    assert!(tokens.contains(&(10, 2, 5, "variable".to_string(), 1 << 3)));
    assert!(tokens.contains(&(8, 24, 5, "method".to_string(), 2 | (1 << 3))));

    server.shutdown();
}

#[test]
fn semantic_tokens_classify_declared_types_properties_and_enum_members() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let source_path = temp.path().join("KindsTokens.pas");
    let source = "unit KindsTokens;\ninterface\ntype\n  TRecord = record\n    Field: Integer;\n  end;\n  TEnum = (First, Second);\n  TIntf = interface\n    procedure Method;\n  end;\n  TClass = class\n    property Value: Integer;\n  end;\nimplementation\nend.\n";
    write_file(&source_path, source);

    let mut server = TestServer::launch();
    let initialize = server.initialize(temp.path(), Value::Null);
    let token_types = initialize["capabilities"]["semanticTokensProvider"]["legend"]["tokenTypes"]
        .as_array()
        .expect("token type legend")
        .iter()
        .map(|token_type| token_type.as_str().expect("token type name"))
        .collect::<Vec<_>>();
    let request_id = RequestId::from("semantic-tokens-kinds".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/semanticTokens/full",
        json!({"textDocument": {"uri": uri(&source_path)}}),
    );
    let response = server.response(&request_id);
    assert!(
        response.error.is_none(),
        "semantic tokens failed: {response:?}"
    );
    let result = response.result.expect("semantic token result");
    let tokens = decoded_semantic_tokens(&result, &token_types);

    assert!(tokens.contains(&(3, 2, 7, "struct".to_string(), 1)));
    assert!(tokens.contains(&(4, 4, 5, "variable".to_string(), 1)));
    assert!(tokens.contains(&(6, 2, 5, "enum".to_string(), 1)));
    assert!(tokens.contains(&(6, 11, 5, "enumMember".to_string(), 5)));
    assert!(tokens.contains(&(6, 18, 6, "enumMember".to_string(), 5)));
    assert!(tokens.contains(&(7, 2, 5, "interface".to_string(), 1)));
    assert!(tokens.contains(&(8, 14, 6, "method".to_string(), 1)));
    assert!(tokens.contains(&(10, 2, 6, "class".to_string(), 1)));
    assert!(tokens.contains(&(11, 13, 5, "property".to_string(), 1)));

    server.shutdown();
}

#[test]
fn initialize_advertises_standard_completion_and_signature_help() {
    let (_temp, main, _provider, _main_source, _provider_source) = standard_workspace();
    let root = main.parent().expect("workspace root");
    let mut server = TestServer::launch();
    let result = server.initialize(root, Value::Null);
    let capabilities = &result["capabilities"];
    assert_eq!(
        capabilities["completionProvider"]["triggerCharacters"],
        json!(["."])
    );
    assert_eq!(capabilities["completionProvider"]["resolveProvider"], true);
    assert_eq!(
        capabilities["signatureHelpProvider"]["triggerCharacters"],
        json!(["(", ","])
    );
    server.shutdown();
}

#[test]
fn completion_snippets_are_negotiated_from_exact_routine_parameters() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let source_path = temp.path().join("CompletionSnippets.pas");
    let source = "unit CompletionSnippets;\ninterface\nprocedure Run(&Value$, A, B: Integer; Optional: string = 'default');\nprocedure Zero;\nimplementation\nprocedure Run(&Value$, A, B: Integer; Optional: string = 'default');\nbegin\nend;\nprocedure Zero;\nbegin\nend;\nprocedure Caller;\nbegin\n  Ru\n  Ze\nend;\nend.\n";
    write_file(&source_path, source);

    for (name, snippet_support, expected_snippet) in [
        (
            "snippet-true",
            Some(true),
            Some("Run(${1:&Value\\$}, ${2:A}, ${3:B}, ${4:Optional})$0"),
        ),
        ("snippet-false", Some(false), None),
        ("snippet-absent", None, None),
    ] {
        let mut server = TestServer::launch();
        server.initialize_with_completion_capabilities(
            temp.path(),
            snippet_support,
            json!([]),
            json!(["plaintext"]),
        );
        let request_id = RequestId::from(name.to_string());
        server.send_request(
            request_id.clone(),
            "textDocument/completion",
            json!({
                "textDocument": {"uri": uri(&source_path)},
                "position": position_after(source, "  Ru", 0)
            }),
        );
        let response = server.response(&request_id);
        assert!(response.error.is_none(), "completion failed: {response:?}");
        let item = response.result.expect("completion result")["items"]
            .as_array()
            .expect("completion items")
            .iter()
            .find(|item| item["label"] == "Run")
            .cloned()
            .expect("Run completion item");
        assert_eq!(item["label"], "Run");
        assert_eq!(item["filterText"], Value::Null);
        assert_eq!(item["sortText"], Value::Null);
        match expected_snippet {
            Some(expected) => {
                assert!(item["insertText"].is_null());
                assert_eq!(item["insertTextFormat"], 2);
                assert_eq!(item["textEdit"]["newText"], expected);
            }
            None => {
                assert!(item["insertText"].is_null());
                assert!(item["insertTextFormat"].is_null());
                assert_eq!(item["textEdit"]["newText"], "Run");
            }
        }
        server.shutdown();
    }
}

#[test]
fn completion_snippets_remain_conservative_for_existing_calls_and_address_of() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let source_path = temp.path().join("SnippetContexts.pas");
    let source = "unit SnippetContexts;\ninterface\ntype\n  TWidget = class\n    procedure Member(Value: Integer);\n  end;\nprocedure Run(Value: Integer);\nprocedure Zero;\nimplementation\nprocedure TWidget.Member(Value: Integer);\nbegin\nend;\nprocedure Run(Value: Integer);\nbegin\nend;\nprocedure Zero;\nbegin\nend;\nprocedure Caller;\nvar\n  Widget: TWidget;\nbegin\n  Widget.Mem;\n  Ze;\n  @Run;\n  Run (* intervening comment *) ();\nend;\nend.\n";
    write_file(&source_path, source);
    let mut server = TestServer::launch();
    server.initialize_with_completion_capabilities(
        temp.path(),
        Some(true),
        json!([]),
        json!(["plaintext"]),
    );

    let request = |server: &mut TestServer, id: &str, needle: &str| {
        let request_id = RequestId::from(id.to_owned());
        server.send_request(
            request_id.clone(),
            "textDocument/completion",
            json!({
                "textDocument": {"uri": uri(&source_path)},
                "position": position_after(source, needle, 0)
            }),
        );
        let response = server.response(&request_id);
        assert!(response.error.is_none(), "completion failed: {response:?}");
        response.result.expect("completion result")["items"]
            .as_array()
            .expect("completion items")
            .to_owned()
    };
    let find = |items: &[Value], label: &str| {
        items
            .iter()
            .find(|item| item["label"] == label)
            .cloned()
            .unwrap_or_else(|| panic!("{label} completion item missing: {items:?}"))
    };

    let member = find(
        &request(&mut server, "member-snippet", "  Widget.Mem"),
        "Member",
    );
    assert_eq!(member["textEdit"]["newText"], "Member(${1:Value})$0");
    assert_eq!(member["insertTextFormat"], 2);

    let zero = find(&request(&mut server, "zero-snippet", "  Ze"), "Zero");
    assert_eq!(zero["textEdit"]["newText"], "Zero()$0");
    assert_eq!(zero["insertTextFormat"], 2);

    let address_of = find(&request(&mut server, "address-of", "  @Run"), "Run");
    assert_eq!(address_of["textEdit"]["newText"], "Run");
    assert!(address_of["insertTextFormat"].is_null());

    let existing_call = find(&request(&mut server, "existing-call", "  Run"), "Run");
    assert_eq!(existing_call["textEdit"]["newText"], "Run");
    assert!(existing_call["insertTextFormat"].is_null());

    server.shutdown();
}

#[test]
fn completion_snippets_stay_plain_for_procedure_value_contexts() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let source_path = temp.path().join("ProcedureValueContexts.pas");
    let source = "unit ProcedureValueContexts;\ninterface\ntype\n  TProc = procedure(Value: Integer);\n  TWidget = class\n    procedure Method(Value: Integer);\n  end;\nprocedure Run(Value: Integer);\nprocedure Take(Callback: TProc);\nimplementation\nprocedure TWidget.Method(Value: Integer);\nbegin\nend;\nprocedure Run(Value: Integer);\nbegin\nend;\nprocedure Take(Callback: TProc);\nbegin\nend;\nprocedure Caller;\nvar\n  Callback: TProc;\n  Widget: TWidget;\nbegin\n  Ru;\n  Callback := Ru;\n  Take(Ru);\n  @Ru;\n  Callback := Widget.Me;\nend;\nend.\n";
    write_file(&source_path, source);

    let mut server = TestServer::launch();
    server.initialize_with_completion_capabilities(
        temp.path(),
        Some(true),
        json!([]),
        json!(["plaintext"]),
    );

    let request = |server: &mut TestServer, id: &str, needle: &str, label: &str| {
        let request_id = RequestId::from(id.to_owned());
        server.send_request(
            request_id.clone(),
            "textDocument/completion",
            json!({
                "textDocument": {"uri": uri(&source_path)},
                "position": position_after(source, needle, 0),
            }),
        );
        let response = server.response(&request_id);
        assert!(response.error.is_none(), "completion failed: {response:?}");
        let result = response.result.expect("completion result");
        result["items"]
            .as_array()
            .expect("completion items")
            .iter()
            .find(|item| item["label"] == label)
            .cloned()
            .unwrap_or_else(|| panic!("{label} completion item missing: {result:?}"))
    };

    let statement = request(&mut server, "procedure-value-statement", "  Ru", "Run");
    assert_eq!(statement["textEdit"]["newText"], "Run(${1:Value})$0");
    assert_eq!(statement["insertTextFormat"], 2);

    for (id, needle, label, replacement) in [
        (
            "procedure-assignment",
            "Callback := Ru",
            "Run",
            "Callback := Run",
        ),
        ("procedure-argument", "Take(Ru", "Run", "Take(Run"),
        ("procedure-address-of", "  @Ru", "Run", "  @Run"),
        (
            "method-pointer-assignment",
            "Callback := Widget.Me",
            "Method",
            "Callback := Widget.Method",
        ),
    ] {
        let item = request(&mut server, id, needle, label);
        assert_eq!(item["textEdit"]["newText"], label);
        assert!(item["insertTextFormat"].is_null());
        assert_eq!(
            apply_completion_item(source, &item),
            source.replace(needle, replacement)
        );
    }

    assert_eq!(
        apply_expanded_completion_item(source, &statement),
        source.replace("  Ru;\n", "  Run(Value);\n")
    );
    server.shutdown();
}

#[test]
fn completion_snippets_require_a_proven_expression_call_role() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let source_path = temp.path().join("CallEligibility.pas");
    let source = "unit CallEligibility;\ninterface\nfunction Make(Value: Integer): Integer;\nfunction Other(Value: Integer): Integer;\nfunction UseResult(Value: Integer): Integer;\nprocedure Run(Value: Integer);\nimplementation\nfunction Make(Value: Integer): Integer;\nbegin\n  Result := Value;\nend;\nfunction Other(Value: Integer): Integer;\nbegin\n  Result := Value;\nend;\nfunction UseResult(Value: Integer): Integer;\nbegin\n  Result := Ma;\nend;\nprocedure Run(Value: Integer);\nbegin\nend;\nprocedure Caller;\nvar\n  Value, Index: Integer;\nbegin\n  Ru;\n  Value := Make(1) + Ma;\n  if Ma > 0 then\n    Value := Value;\n  for Index := 1 to Ma do\n    Value := Value;\n  Ma := Value;\nend;\nend.\n";
    write_file(&source_path, source);

    let mut server = TestServer::launch();
    server.initialize_with_completion_capabilities(
        temp.path(),
        Some(true),
        json!([]),
        json!(["plaintext"]),
    );

    let request =
        |server: &mut TestServer, id: &str, needle: &str, occurrence: usize, label: &str| {
            let request_id = RequestId::from(id.to_owned());
            server.send_request(
                request_id.clone(),
                "textDocument/completion",
                json!({
                    "textDocument": {"uri": uri(&source_path)},
                    "position": position_after(source, needle, occurrence),
                }),
            );
            let response = server.response(&request_id);
            assert!(response.error.is_none(), "completion failed: {response:?}");
            let result = response.result.expect("completion result");
            result["items"]
                .as_array()
                .expect("completion items")
                .iter()
                .find(|item| item["label"] == label)
                .cloned()
                .unwrap_or_else(|| panic!("{label} completion item missing: {result:?}"))
        };

    let statement = request(&mut server, "eligibility-statement", "  Ru", 0, "Run");
    assert_eq!(statement["textEdit"]["newText"], "Run(${1:Value})$0");
    assert_eq!(statement["insertTextFormat"], 2);

    let nested = request(
        &mut server,
        "eligibility-nested-expression",
        " + Ma",
        0,
        "Make",
    );
    assert_eq!(nested["textEdit"]["newText"], "Make(${1:Value})$0");
    assert_eq!(
        apply_expanded_completion_item(source, &nested),
        source.replace(" + Ma;\n", " + Make(Value);\n")
    );

    for (id, needle) in [
        ("eligibility-condition", "if Ma"),
        ("eligibility-loop-end", "to Ma"),
    ] {
        let item = request(&mut server, id, needle, 0, "Make");
        assert_eq!(item["textEdit"]["newText"], "Make(${1:Value})$0");
        assert_eq!(item["insertTextFormat"], 2);
    }

    let result_rhs = request(
        &mut server,
        "eligibility-function-result-rhs",
        "Result := Ma",
        0,
        "Make",
    );
    assert_eq!(result_rhs["textEdit"]["newText"], "Make(${1:Value})$0");
    assert_eq!(result_rhs["insertTextFormat"], 2);

    let result_assignment = request(
        &mut server,
        "eligibility-result-assignment",
        "  Ma",
        0,
        "Make",
    );
    assert_eq!(result_assignment["textEdit"]["newText"], "Make");
    assert!(result_assignment["insertTextFormat"].is_null());
    assert_eq!(
        apply_completion_item(source, &result_assignment),
        source.replace("  Ma := Value;\n", "  Make := Value;\n")
    );

    server.shutdown();
}

#[test]
fn completion_snippets_preserve_inner_expected_types_and_index_destinations() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let source_path = temp.path().join("NestedExpectedCompletion.pas");
    let source = r#"unit NestedExpectedCompletion;
interface
type
  TProc = procedure(Value: Integer);
  TProcArray = array[0..1] of TProc;
  TIntArray = array[0..1] of Integer;
function Count(Callback: TProc): Integer;
function Make(Value: Integer): Integer;
procedure Consume(Value: Integer);
function Opaque(Callback: TUnresolved): Integer;
procedure Run(Value: Integer);
implementation
function Count(Callback: TProc): Integer;
begin
  Result := 1;
end;
function Make(Value: Integer): Integer;
begin
  Result := Value;
end;
procedure Consume(Value: Integer);
begin
end;
function Opaque(Callback: TUnresolved): Integer;
begin
  Result := 1;
end;
procedure Run(Value: Integer);
begin
end;
procedure Caller;
var
  Value, Index: Integer;
  Callbacks: TProcArray;
  Values: TIntArray;
begin
  Value := Count(Ru);
  Consume(Count(Ru));
  Consume(Ma);
  Consume(Opaque(Ru));
  Value := Ma;
  Callbacks[Index] := Ru;
  Callbacks[0] := Ru;
  Values[Index] := Ma;
  Values[0] := Ma;
  Unknown[Index] := Ru;
end;
end.
"#;
    write_file(&source_path, source);

    let mut server = TestServer::launch();
    server.initialize_with_completion_capabilities(
        temp.path(),
        Some(true),
        json!([]),
        json!(["plaintext"]),
    );

    let request =
        |server: &mut TestServer, id: &str, needle: &str, occurrence: usize, label: &str| {
            let request_id = RequestId::from(id.to_owned());
            server.send_request(
                request_id.clone(),
                "textDocument/completion",
                json!({
                    "textDocument": {"uri": uri(&source_path)},
                    "position": position_after(source, needle, occurrence),
                }),
            );
            let response = server.response(&request_id);
            assert!(response.error.is_none(), "completion failed: {response:?}");
            let result = response.result.expect("completion result");
            result["items"]
                .as_array()
                .expect("completion items")
                .iter()
                .find(|item| item["label"] == label)
                .cloned()
                .unwrap_or_else(|| panic!("{label} completion item missing: {result:?}"))
        };
    let mut failures = Vec::new();

    for (id, needle, occurrence, original, replacement) in [
        (
            "nested-procedure-value",
            "Count(Ru",
            0,
            "Value := Count(Ru);",
            "Value := Count(Run);",
        ),
        (
            "double-nested-procedure-value",
            "Count(Ru",
            1,
            "Consume(Count(Ru));",
            "Consume(Count(Run));",
        ),
        (
            "unknown-nested-expected-value",
            "Opaque(Ru",
            0,
            "Consume(Opaque(Ru));",
            "Consume(Opaque(Run));",
        ),
    ] {
        let item = request(&mut server, id, needle, occurrence, "Run");
        if item["textEdit"]["newText"] != "Run" || !item["insertTextFormat"].is_null() {
            failures.push(format!(
                "{id}: expected plain Run, got {}",
                item["textEdit"]["newText"]
            ));
        }
        let expanded = apply_completion_item(source, &item);
        let expected = source.replacen(original, replacement, 1);
        if expanded != expected {
            failures.push(format!("{id}: expanded source was {expanded:?}"));
        }
    }

    for (id, needle, occurrence) in [
        ("nonprocedural-nested-value", "Consume(Ma", 0),
        ("nonprocedural-indexed-value", "Values[Index] := Ma", 0),
        ("nonprocedural-literal-indexed-value", "Values[0] := Ma", 0),
        ("genuine-function-rvalue", "Value := Ma", 0),
    ] {
        let item = request(&mut server, id, needle, occurrence, "Make");
        if item["textEdit"]["newText"] != "Make(${1:Value})$0" || item["insertTextFormat"] != 2 {
            failures.push(format!(
                "{id}: expected Make snippet, got {}",
                item["textEdit"]["newText"]
            ));
        }
    }

    for (id, needle, occurrence, replacement) in [
        (
            "indexed-procedure-value",
            "Callbacks[Index] := Ru",
            0,
            "Callbacks[Index] := Run",
        ),
        (
            "literal-indexed-procedure-value",
            "Callbacks[0] := Ru",
            0,
            "Callbacks[0] := Run",
        ),
    ] {
        let item = request(&mut server, id, needle, occurrence, "Run");
        if item["textEdit"]["newText"] != "Run" || !item["insertTextFormat"].is_null() {
            failures.push(format!(
                "{id}: expected plain Run, got {}",
                item["textEdit"]["newText"]
            ));
        }
        let expanded = apply_completion_item(source, &item);
        let expected = source.replace(needle, replacement);
        if expanded != expected {
            failures.push(format!("{id}: expanded source was {expanded:?}"));
        }
    }

    let unproven = request(
        &mut server,
        "unproven-indexed-destination",
        "Unknown[Index] := Ru",
        0,
        "Run",
    );
    if unproven["textEdit"]["newText"] != "Run" || !unproven["insertTextFormat"].is_null() {
        failures.push(format!(
            "unproven-indexed-destination: expected plain Run, got {}",
            unproven["textEdit"]["newText"]
        ));
    }
    let expanded = apply_completion_item(source, &unproven);
    let expected = source.replace("Unknown[Index] := Ru", "Unknown[Index] := Run");
    if expanded != expected {
        failures.push(format!(
            "unproven-indexed-destination: expanded source was {expanded:?}"
        ));
    }

    assert!(failures.is_empty(), "completion regressions: {failures:#?}");
    server.shutdown();
}

#[test]
fn completion_snippets_project_full_indexed_destination_types() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let source_path = temp.path().join("MultiIndexCompletion.pas");
    let source = r#"unit MultiIndexCompletion;
interface
type
  TProc = procedure(Value: Integer);
  TProcArray = array[0..1] of TProc;
  TMatrix = array[0..1] of TProcArray;
  TDeclaredMatrix = array[0..1, 0..1] of TProc;
  TScalarMatrix = array[0..1, 0..1] of Integer;
  TCycleA = array[0..1] of TCycleB;
  TCycleB = array[0..1] of TCycleA;
function Make(Value: Integer): Integer;
procedure Run(Value: Integer);
implementation
function Make(Value: Integer): Integer;
begin
  Result := Value;
end;
procedure Run(Value: Integer);
begin
end;
procedure Caller;
var
  Matrix: TMatrix;
  DeclaredMatrix: TDeclaredMatrix;
  ScalarMatrix: TScalarMatrix;
  Cycle: TCycleA;
  Index: Integer;
begin
  Matrix[Index,0] := Ru;
  Matrix[Index][0] := Ru;
  DeclaredMatrix[Index,0] := Ru;
  ScalarMatrix[Index,0] := Ma;
  Matrix[Index,0,1] := Ru;
  Matrix[Index,] := Ru;
  Cycle[Index,0] := Ru;
end;
end.
"#;
    write_file(&source_path, source);

    let mut server = TestServer::launch();
    server.initialize_with_completion_capabilities(
        temp.path(),
        Some(true),
        json!([]),
        json!(["plaintext"]),
    );

    let request =
        |server: &mut TestServer, id: &str, needle: &str, occurrence: usize, label: &str| {
            let request_id = RequestId::from(id.to_owned());
            server.send_request(
                request_id.clone(),
                "textDocument/completion",
                json!({
                    "textDocument": {"uri": uri(&source_path)},
                    "position": position_after(source, needle, occurrence),
                }),
            );
            let response = server.response(&request_id);
            assert!(response.error.is_none(), "completion failed: {response:?}");
            let result = response.result.expect("completion result");
            result["items"]
                .as_array()
                .expect("completion items")
                .iter()
                .find(|item| item["label"] == label)
                .cloned()
                .unwrap_or_else(|| panic!("{label} completion item missing: {result:?}"))
        };
    let mut failures = Vec::new();

    for (id, needle, replacement) in [
        (
            "named-comma-indices",
            "Matrix[Index,0] := Ru",
            "Matrix[Index,0] := Run",
        ),
        (
            "chained-indices",
            "Matrix[Index][0] := Ru",
            "Matrix[Index][0] := Run",
        ),
        (
            "declared-multidimensional",
            "DeclaredMatrix[Index,0] := Ru",
            "DeclaredMatrix[Index,0] := Run",
        ),
        (
            "excess-indices",
            "Matrix[Index,0,1] := Ru",
            "Matrix[Index,0,1] := Run",
        ),
        (
            "malformed-indices",
            "Matrix[Index,] := Ru",
            "Matrix[Index,] := Run",
        ),
        (
            "cyclic-array-indices",
            "Cycle[Index,0] := Ru",
            "Cycle[Index,0] := Run",
        ),
    ] {
        let item = request(&mut server, id, needle, 0, "Run");
        if item["textEdit"]["newText"] != "Run" || !item["insertTextFormat"].is_null() {
            failures.push(format!(
                "{id}: expected plain Run, got {}",
                item["textEdit"]["newText"]
            ));
        }
        let expanded = apply_completion_item(source, &item);
        let expected = source.replacen(needle, replacement, 1);
        if expanded != expected {
            failures.push(format!(
                "{id}: expanded source was {expanded:?}, expected {expected:?}"
            ));
        }
    }

    let scalar = request(
        &mut server,
        "scalar-element",
        "ScalarMatrix[Index,0] := Ma",
        0,
        "Make",
    );
    if scalar["textEdit"]["newText"] != "Make(${1:Value})$0" || scalar["insertTextFormat"] != 2 {
        failures.push(format!(
            "scalar-element: expected Make snippet, got {}",
            scalar["textEdit"]["newText"]
        ));
    }

    assert!(failures.is_empty(), "completion regressions: {failures:#?}");
    server.shutdown();
}

#[test]
fn completion_snippets_fail_closed_for_terminal_type_aliases() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let source_path = temp.path().join("TerminalAliasCompletion.pas");
    let source = r#"unit TerminalAliasCompletion;
interface
type
  TProc = procedure(Value: Integer);
  TProcAlias = TProc;
  TProcAliasChain = TProcAlias;
  TProcRow = array[0..1] of TProcAliasChain;
  TProcMatrix = array[0..1] of TProcRow;
  TScalarLeaf = Integer;
  TScalarLeafAlias = TScalarLeaf;
  TScalarRow = array[0..1] of TScalarLeafAlias;
  TScalarMatrix = array[0..1] of TScalarRow;
  TResidualBase = array[0..1] of Integer;
  TResidualAlias = TResidualBase;
  TResidualOuter = array[0..1] of TResidualAlias;
  TUnknownAlias = TMissingType;
  TUnknownRow = array[0..1] of TUnknownAlias;
  TCycleA = array[0..1] of TCycleB;
  TCycleB = TCycleA;
function Make(Value: Integer): Integer;
procedure Run(Value: Integer);
implementation
function Make(Value: Integer): Integer;
begin
  Result := Value;
end;
procedure Run(Value: Integer);
begin
end;
procedure Caller;
var
  ProcMatrix: TProcMatrix;
  ScalarMatrix: TScalarMatrix;
  Residual: TResidualOuter;
  Unknown: TUnknownRow;
  Cycle: TCycleA;
  Index: Integer;
begin
  ProcMatrix[Index,0] := Ru;
  ScalarMatrix[Index,0] := Ma;
  Residual[Index] := Ma;
  Unknown[Index] := Ru;
  Cycle[Index] := Ru;
end;
end.
"#;
    write_file(&source_path, source);

    let mut server = TestServer::launch();
    server.initialize_with_completion_capabilities(
        temp.path(),
        Some(true),
        json!([]),
        json!(["plaintext"]),
    );
    let request = |server: &mut TestServer, id: &str, needle: &str, label: &str| {
        let request_id = RequestId::from(id.to_owned());
        server.send_request(
            request_id.clone(),
            "textDocument/completion",
            json!({
                "textDocument": {"uri": uri(&source_path)},
                "position": position_after(source, needle, 0),
            }),
        );
        let response = server.response(&request_id);
        assert!(response.error.is_none(), "completion failed: {response:?}");
        let result = response.result.expect("completion result");
        result["items"]
            .as_array()
            .expect("completion items")
            .iter()
            .find(|item| item["label"] == label)
            .cloned()
            .unwrap_or_else(|| panic!("{label} completion item missing: {result:?}"))
    };

    let mut failures = Vec::new();
    for (id, needle, replacement, label) in [
        (
            "procedural-terminal-alias-chain",
            "ProcMatrix[Index,0] := Ru",
            "ProcMatrix[Index,0] := Run",
            "Run",
        ),
        (
            "residual-array-terminal-alias",
            "Residual[Index] := Ma",
            "Residual[Index] := Make",
            "Make",
        ),
        (
            "unknown-terminal-alias",
            "Unknown[Index] := Ru",
            "Unknown[Index] := Run",
            "Run",
        ),
        (
            "cyclic-terminal-alias",
            "Cycle[Index] := Ru",
            "Cycle[Index] := Run",
            "Run",
        ),
    ] {
        let item = request(&mut server, id, needle, label);
        if item["textEdit"]["newText"] != label || !item["insertTextFormat"].is_null() {
            failures.push(format!("{id}: expected plain {label}, got {item}"));
        }
        let expanded = apply_completion_item(source, &item);
        let expected = source.replacen(needle, replacement, 1);
        if expanded != expected {
            failures.push(format!("{id}: expanded source was {expanded:?}"));
        }
    }

    let scalar = request(
        &mut server,
        "scalar-terminal-alias-chain",
        "ScalarMatrix[Index,0] := Ma",
        "Make",
    );
    if scalar["textEdit"]["newText"] != "Make(${1:Value})$0" || scalar["insertTextFormat"] != 2 {
        failures.push(format!(
            "scalar-terminal-alias-chain: expected Make snippet, got {scalar}"
        ));
    }
    let expanded = apply_expanded_completion_item(source, &scalar);
    let expected = source.replacen(
        "ScalarMatrix[Index,0] := Ma",
        "ScalarMatrix[Index,0] := Make(Value)",
        1,
    );
    if expanded != expected {
        failures.push(format!(
            "scalar-terminal-alias-chain: expanded source was {expanded:?}"
        ));
    }

    assert!(failures.is_empty(), "completion regressions: {failures:#?}");

    server.shutdown();
}

#[test]
fn completion_snippets_stay_plain_for_non_expression_syntax_roles() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let cases = [
        (
            "label",
            "unit LabelRole;\ninterface\nprocedure Run(Value: Integer);\nimplementation\nprocedure Run(Value: Integer);\nbegin\nend;\nprocedure Caller;\nbegin\n  goto Ru;\nend;\nend.\n",
            "goto Ru",
        ),
        (
            "loop-target",
            "unit LoopTargetRole;\ninterface\nprocedure Run(Value: Integer);\nimplementation\nprocedure Run(Value: Integer);\nbegin\nend;\nprocedure Caller;\nvar\n  Value: Integer;\nbegin\n  for Ru := 1 to 2 do\n    Value := Value;\nend;\nend.\n",
            "for Ru",
        ),
        (
            "left-qualification",
            "unit LeftQualificationRole;\ninterface\nprocedure Run(Value: Integer);\nimplementation\nprocedure Run(Value: Integer);\nbegin\nend;\nprocedure Caller;\nbegin\n  Ru.Member;\nend;\nend.\n",
            "  Ru",
        ),
        (
            "uncertain",
            "unit UncertainRole;\ninterface\nprocedure Run(Value: Integer);\nimplementation\nprocedure Run(Value: Integer);\nbegin\nend;\nprocedure Caller;\nbegin\n  Ru ???;\nend;\nend.\n",
            "  Ru",
        ),
    ];

    for (id, source, needle) in cases {
        let source_path = temp.path().join(format!("{id}.pas"));
        write_file(&source_path, source);
        let mut server = TestServer::launch();
        server.initialize_with_completion_capabilities(
            temp.path(),
            Some(true),
            json!([]),
            json!(["plaintext"]),
        );
        let request_id = RequestId::from(format!("non-expression-{id}"));
        server.send_request(
            request_id.clone(),
            "textDocument/completion",
            json!({
                "textDocument": {"uri": uri(&source_path)},
                "position": position_after(source, needle, 0),
            }),
        );
        let response = server.response(&request_id);
        assert!(response.error.is_none(), "completion failed: {response:?}");
        let item = response.result.expect("completion result")["items"]
            .as_array()
            .expect("completion items")
            .iter()
            .find(|item| item["label"] == "Run")
            .cloned()
            .unwrap_or_else(|| panic!("Run completion item missing for {id}"));
        assert_eq!(item["textEdit"]["newText"], "Run");
        assert!(item["insertTextFormat"].is_null());
        server.shutdown();
    }
}

#[test]
fn completion_snippets_keep_utf16_mid_token_ranges_and_crlf_source() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let source_path = temp.path().join("CrLfSnippets.pas");
    let source = concat!(
        "unit CrLfSnippets;\r\n",
        "interface\r\n",
        "procedure Run(Value: Integer);\r\n",
        "implementation\r\n",
        "procedure Run(Value: Integer);\r\n",
        "begin\r\n",
        "end;\r\n",
        "procedure Caller;\r\n",
        "begin\r\n",
        "  (* 😀 *) RuSuffix\r\n",
        "end;\r\n",
        "end.\r\n",
    );
    write_file(&source_path, source);

    let mut server = TestServer::launch();
    server.initialize_with_completion_capabilities(
        temp.path(),
        Some(true),
        json!([]),
        json!(["plaintext"]),
    );
    let request_id = RequestId::from("crlf-mid-token-snippet".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/completion",
        json!({
            "textDocument": {"uri": uri(&source_path)},
            "position": position_after(source, "  (* 😀 *) Ru", 0)
        }),
    );
    let response = server.response(&request_id);
    assert!(response.error.is_none(), "completion failed: {response:?}");
    let item = response.result.expect("completion result")["items"]
        .as_array()
        .expect("completion items")
        .iter()
        .find(|item| item["label"] == "Run")
        .cloned()
        .expect("Run completion item");
    assert_eq!(item["textEdit"]["newText"], "Run(${1:Value})$0");
    assert_eq!(item["insertTextFormat"], 2);
    assert_eq!(
        item["textEdit"]["range"],
        json!({
            "start": {"line": 9, "character": 11},
            "end": {"line": 9, "character": 19}
        })
    );
    server.shutdown();
}

#[test]
fn completion_snippets_stay_plain_for_ambiguous_overloads() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let source_path = temp.path().join("OverloadSnippets.pas");
    let source = "unit OverloadSnippets;\ninterface\nprocedure Pick(NumberValue: Integer); overload;\nprocedure Pick(TextValue: string); overload;\nimplementation\nprocedure Pick(NumberValue: Integer);\nbegin\nend;\nprocedure Pick(TextValue: string);\nbegin\nend;\nprocedure Caller;\nbegin\n  Pi;\nend;\nend.\n";
    write_file(&source_path, source);
    let mut server = TestServer::launch();
    server.initialize_with_completion_capabilities(
        temp.path(),
        Some(true),
        json!([]),
        json!(["plaintext"]),
    );
    let request_id = RequestId::from("ambiguous-overload-snippet".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/completion",
        json!({
            "textDocument": {"uri": uri(&source_path)},
            "position": position_after(source, "  Pi", 0)
        }),
    );
    let response = server.response(&request_id);
    assert!(response.error.is_none(), "completion failed: {response:?}");
    let item = response.result.expect("completion result")["items"]
        .as_array()
        .expect("completion items")
        .iter()
        .find(|item| item["label"] == "Pick")
        .cloned()
        .expect("Pick completion item");
    assert_eq!(item["textEdit"]["newText"], "Pick");
    assert!(item["insertTextFormat"].is_null());
    server.shutdown();
}

#[test]
fn completion_snippets_stay_plain_for_cross_unit_overloads() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let provider_path = temp.path().join("CrossUnitOverloadProvider.pas");
    let main_path = temp.path().join("CrossUnitOverloadConsumer.pas");
    let provider_source = "unit CrossUnitOverloadProvider;\ninterface\nprocedure Pick(NumberValue: Integer); overload;\nprocedure Pick(TextValue: string); overload;\nimplementation\nprocedure Pick(NumberValue: Integer);\nbegin\nend;\nprocedure Pick(TextValue: string);\nbegin\nend;\nend.\n";
    let main_source = "unit CrossUnitOverloadConsumer;\ninterface\nuses CrossUnitOverloadProvider;\nimplementation\nprocedure Caller;\nbegin\n  Pi\nend;\nend.\n";
    write_file(&provider_path, provider_source);
    write_file(&main_path, main_source);

    let mut server = TestServer::launch();
    server.initialize_with_completion_capabilities(
        temp.path(),
        Some(true),
        json!([]),
        json!(["plaintext"]),
    );
    let request_id = RequestId::from("cross-unit-overload-snippet".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/completion",
        json!({
            "textDocument": {"uri": uri(&main_path)},
            "position": position_after(main_source, "  Pi", 0),
        }),
    );
    let response = server.response(&request_id);
    assert!(response.error.is_none(), "completion failed: {response:?}");
    let item = response.result.expect("completion result")["items"]
        .as_array()
        .expect("completion items")
        .iter()
        .find(|item| item["label"] == "Pick")
        .cloned()
        .expect("cross-unit Pick completion item");
    assert_eq!(item["textEdit"]["newText"], "Pick");
    assert!(item["insertTextFormat"].is_null());
    assert_eq!(
        apply_completion_item(main_source, &item),
        main_source.replace("  Pi\n", "  Pick\n")
    );
    server.shutdown();
}

#[test]
fn completion_snippets_do_not_add_statement_terminators_inside_expressions() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let source_path = temp.path().join("NestedSnippet.pas");
    let source = "unit NestedSnippet;\ninterface\nfunction Make(Value: Integer): Integer;\nimplementation\nfunction Make(Value: Integer): Integer;\nbegin\n  Result := Value;\nend;\nprocedure Caller;\nvar\n  Value: Integer;\nbegin\n  Value := Make(1) + Ma;\nend;\nend.\n";
    write_file(&source_path, source);
    let mut server = TestServer::launch();
    server.initialize_with_completion_capabilities(
        temp.path(),
        Some(true),
        json!([]),
        json!(["plaintext"]),
    );
    let request_id = RequestId::from("nested-expression-snippet".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/completion",
        json!({
            "textDocument": {"uri": uri(&source_path)},
            "position": position_after(source, " + Ma", 0)
        }),
    );
    let response = server.response(&request_id);
    assert!(response.error.is_none(), "completion failed: {response:?}");
    let item = response.result.expect("completion result")["items"]
        .as_array()
        .expect("completion items")
        .iter()
        .find(|item| item["label"] == "Make")
        .cloned()
        .expect("Make completion item");
    assert_eq!(item["textEdit"]["newText"], "Make(${1:Value})$0");
    assert!(
        !item["textEdit"]["newText"]
            .as_str()
            .expect("snippet text")
            .contains(';')
    );
    server.shutdown();
}

#[test]
fn completion_snippets_stay_plain_in_routine_declarations() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let source_path = temp.path().join("DeclarationSnippet.pas");
    let source = "unit DeclarationSnippet;\ninterface\nprocedure Run(Value: Integer);\nprocedure Ru;\nimplementation\nprocedure Run(Value: Integer);\nbegin\nend;\nprocedure Ru;\nbegin\nend;\nend.\n";
    write_file(&source_path, source);
    let mut server = TestServer::launch();
    server.initialize_with_completion_capabilities(
        temp.path(),
        Some(true),
        json!([]),
        json!(["plaintext"]),
    );
    let request_id = RequestId::from("declaration-snippet".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/completion",
        json!({
            "textDocument": {"uri": uri(&source_path)},
            "position": position_after(source, "procedure Ru", 0)
        }),
    );
    let response = server.response(&request_id);
    assert!(response.error.is_none(), "completion failed: {response:?}");
    let item = response.result.expect("completion result")["items"]
        .as_array()
        .expect("completion items")
        .iter()
        .find(|item| item["label"] == "Run")
        .cloned()
        .expect("Run completion item");
    assert_eq!(item["textEdit"]["newText"], "Run");
    assert!(item["insertTextFormat"].is_null());
    server.shutdown();
}

#[test]
fn completion_resolution_defers_negotiated_fields_and_restores_stable_item() {
    let temp = tempfile::tempdir().unwrap();
    let source_path = temp.path().join("DeferredCompletion.pas");
    let source = "unit DeferredCompletion;\ninterface\n/// <summary>Returns the value.</summary>\n/// <param name=\"Name\">Lookup name.</param>\nfunction Documented(Name: string): Integer;\nimplementation\nfunction Documented(Name: string): Integer;\nbegin\n  Result := 1;\nend;\nprocedure Caller;\nbegin\n  Doc\nend;\nend.\n";
    write_file(&source_path, source);

    let mut eager_server = TestServer::launch();
    let eager_initialize = eager_server.initialize_with_completion_resolve_properties(
        temp.path(),
        json!([]),
        json!(["plaintext"]),
    );
    assert_eq!(
        eager_initialize["capabilities"]["completionProvider"]["resolveProvider"],
        true
    );
    let eager_id = RequestId::from("completion-eager".to_string());
    eager_server.send_request(
        eager_id.clone(),
        "textDocument/completion",
        json!({
            "textDocument": {"uri": uri(&source_path)},
            "position": position_after(source, "  Doc", 0)
        }),
    );
    let eager = eager_server.response(&eager_id);
    assert!(eager.error.is_none(), "eager completion failed: {eager:?}");
    let eager_item = eager.result.expect("eager completion result")["items"]
        .as_array()
        .expect("eager completion items")
        .iter()
        .find(|item| item["label"] == "Documented")
        .cloned()
        .expect("eager documented item");
    assert!(eager_item["documentation"].is_string());
    assert!(
        eager_item["detail"]
            .as_str()
            .is_some_and(|detail| detail.contains("Documented"))
    );
    assert!(eager_item["data"].is_null());
    eager_server.shutdown();

    let mut deferred_server = TestServer::launch();
    let initialize = deferred_server.initialize_with_completion_capabilities(
        temp.path(),
        Some(true),
        json!(["documentation", "detail"]),
        json!(["markdown", "plaintext"]),
    );
    assert_eq!(
        initialize["capabilities"]["completionProvider"]["resolveProvider"],
        true
    );
    let request_id = RequestId::from("completion-deferred".to_string());
    deferred_server.send_request(
        request_id.clone(),
        "textDocument/completion",
        json!({
            "textDocument": {"uri": uri(&source_path)},
            "position": position_after(source, "  Doc", 0)
        }),
    );
    let response = deferred_server.response(&request_id);
    assert!(
        response.error.is_none(),
        "deferred completion failed: {response:?}"
    );
    let initial = response.result.expect("deferred completion result");
    let item = initial["items"]
        .as_array()
        .expect("deferred completion items")
        .iter()
        .find(|item| item["label"] == "Documented")
        .cloned()
        .expect("deferred documented item");
    assert!(item["documentation"].is_null());
    assert!(item["detail"].is_null());
    assert!(item["data"].is_object());
    assert_eq!(item["textEdit"]["newText"], "Documented(${1:Name})$0");
    assert_eq!(item["insertTextFormat"], 2);
    let original_edit = item["textEdit"].clone();
    let original_kind = item["kind"].clone();
    let original_insert_text = item["insertText"].clone();
    let original_filter_text = item["filterText"].clone();
    let original_additional_edits = item["additionalTextEdits"].clone();
    let mut resolve_item = item.clone();
    resolve_item["label"] = json!("ForgedLabel");
    resolve_item["detail"] = json!("Forged detail");
    resolve_item["textEdit"]["newText"] = json!("FORGED_EDIT");
    resolve_item["sortText"] = json!("forged-sort");
    resolve_item["kind"] = json!(1);
    resolve_item["insertText"] = json!("FORGED_INSERT");
    resolve_item["filterText"] = json!("forged-filter");
    resolve_item["additionalTextEdits"] = json!([]);

    let resolve_id = RequestId::from("completion-resolve".to_string());
    deferred_server.send_request(resolve_id.clone(), "completionItem/resolve", resolve_item);
    let resolved = deferred_server.response(&resolve_id);
    assert!(
        resolved.error.is_none(),
        "completion resolve failed: {resolved:?}"
    );
    let resolved = resolved.result.expect("resolved completion item");
    assert_eq!(resolved["label"], "Documented");
    assert_eq!(resolved["textEdit"], original_edit);
    assert_eq!(resolved["textEdit"]["newText"], "Documented(${1:Name})$0");
    assert_eq!(resolved["insertTextFormat"], 2);
    assert_eq!(resolved["kind"], original_kind);
    assert_eq!(resolved["insertText"], original_insert_text);
    assert_eq!(resolved["filterText"], original_filter_text);
    assert_eq!(resolved["additionalTextEdits"], original_additional_edits);
    assert_eq!(resolved["documentation"]["kind"], "markdown");
    assert!(
        resolved["documentation"]["value"]
            .as_str()
            .is_some_and(|value| value.contains("Returns the value."))
    );
    assert!(
        resolved["detail"]
            .as_str()
            .is_some_and(|detail| detail.contains("Documented"))
    );
    deferred_server.shutdown();
}

#[test]
fn completion_resolution_accepts_unchanged_open_source_after_unrelated_overlay_edit() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let source_path = temp.path().join("OpenDeferredCompletion.pas");
    let unrelated_path = temp.path().join("Unrelated.pas");
    let source = "unit OpenDeferredCompletion;\ninterface\n/// <summary>Returns the open value.</summary>\nfunction OpenDocumented: Integer;\nimplementation\nfunction OpenDocumented: Integer;\nbegin\n  Result := 1;\nend;\nprocedure Caller;\nbegin\n  OpenDoc\nend;\nend.\n";
    write_file(&source_path, source);
    write_file(
        &unrelated_path,
        "unit Unrelated; interface implementation end.\n",
    );

    let mut server = TestServer::launch();
    server.initialize_with_completion_capabilities(
        temp.path(),
        Some(true),
        json!(["documentation", "detail"]),
        json!(["markdown"]),
    );
    server.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri(&source_path),
                "languageId": "pascal",
                "version": 1,
                "text": source
            }
        }),
    );
    let completion_id = RequestId::from("open-completion".to_string());
    server.send_request(
        completion_id.clone(),
        "textDocument/completion",
        json!({
            "textDocument": {"uri": uri(&source_path)},
            "position": position_after(source, "  OpenDoc", 0)
        }),
    );
    let initial = server
        .response(&completion_id)
        .result
        .expect("open completion result");
    let item = initial["items"]
        .as_array()
        .expect("open completion items")
        .iter()
        .find(|item| item["label"] == "OpenDocumented")
        .cloned()
        .expect("open documented item");

    server.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri(&unrelated_path),
                "languageId": "pascal",
                "version": 1,
                "text": "unit Unrelated; interface implementation end.\n"
            }
        }),
    );
    let resolve_id = RequestId::from("open-completion-resolve".to_string());
    server.send_request(resolve_id.clone(), "completionItem/resolve", item);
    let response = server.response(&resolve_id);
    assert!(
        response.error.is_none(),
        "open resolve failed: {response:?}"
    );
    let resolved = response.result.expect("open resolved item");
    assert_eq!(resolved["label"], "OpenDocumented");
    assert!(
        resolved["documentation"]["value"]
            .as_str()
            .is_some_and(|value| value.contains("open value"))
    );
    server.shutdown();
}

#[test]
fn completion_resolution_rejects_a_project_context_switch() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let main_path = temp.path().join("ContextCompletion.pas");
    let project_a = temp.path().join("A.dproj");
    let project_b = temp.path().join("B.dproj");
    let source = "unit ContextCompletion;\ninterface\n/// <summary>Context-bound value.</summary>\nfunction ContextDocumented: Integer;\nimplementation\nfunction ContextDocumented: Integer;\nbegin\n  Result := 1;\nend;\nprocedure Caller;\nbegin\n  ContextDoc\nend;\nend.\n";
    write_file(&main_path, source);
    for project in [&project_a, &project_b] {
        write_file(
            project,
            "<Project><PropertyGroup><MainSource>ContextCompletion.pas</MainSource></PropertyGroup></Project>",
        );
    }

    let mut server = TestServer::launch();
    server.initialize_with_completion_resolve_properties(
        temp.path(),
        json!(["documentation", "detail"]),
        json!(["markdown"]),
    );
    let select_a_id = RequestId::from("context-select-a".to_string());
    server.send_request(
        select_a_id.clone(),
        "pascal/selectProject",
        json!({
            "textDocument": {"uri": uri(&main_path)},
            "projectUri": uri(&project_a)
        }),
    );
    assert!(server.response(&select_a_id).error.is_none());

    let completion_id = RequestId::from("context-completion".to_string());
    server.send_request(
        completion_id.clone(),
        "textDocument/completion",
        json!({
            "textDocument": {"uri": uri(&main_path)},
            "position": position_after(source, "  ContextDoc", 0)
        }),
    );
    let initial = server
        .response(&completion_id)
        .result
        .expect("context completion result");
    let item = initial["items"]
        .as_array()
        .expect("context completion items")
        .iter()
        .find(|item| item["label"] == "ContextDocumented")
        .cloned()
        .expect("context documented item");

    let select_b_id = RequestId::from("context-select-b".to_string());
    server.send_request(
        select_b_id.clone(),
        "pascal/selectProject",
        json!({
            "textDocument": {"uri": uri(&main_path)},
            "projectUri": uri(&project_b)
        }),
    );
    assert!(server.response(&select_b_id).error.is_none());

    let resolve_id = RequestId::from("context-completion-resolve".to_string());
    server.send_request(resolve_id.clone(), "completionItem/resolve", item);
    let response = server.response(&resolve_id);
    let error = response
        .error
        .expect("context switch must invalidate completion resolution");
    assert_eq!(error.code, -32803);
    server.shutdown();
}

#[test]
fn completion_resolution_rejects_a_provider_disk_change_without_watcher_notification() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let main_path = temp.path().join("Main.pas");
    let provider_path = temp.path().join("Provider.pas");
    let main_source = "unit Main;\ninterface\nuses Provider;\nimplementation\nprocedure Caller;\nbegin\n  ProviderDoc\nend;\nend.\n";
    let provider_source = "unit Provider;\ninterface\n/// <summary>Provider documentation.</summary>\nfunction ProviderDocumented: Integer;\nimplementation\nfunction ProviderDocumented: Integer;\nbegin\n  Result := 1;\nend;\nend.\n";
    write_file(&main_path, main_source);
    write_file(&provider_path, provider_source);

    let mut server = TestServer::launch();
    server.initialize_with_completion_resolve_properties(
        temp.path(),
        json!(["documentation", "detail"]),
        json!(["plaintext"]),
    );
    let completion_id = RequestId::from("provider-completion".to_string());
    server.send_request(
        completion_id.clone(),
        "textDocument/completion",
        json!({
            "textDocument": {"uri": uri(&main_path)},
            "position": position_after(main_source, "  ProviderDoc", 0)
        }),
    );
    let initial = server
        .response(&completion_id)
        .result
        .expect("provider completion result");
    let item = initial["items"]
        .as_array()
        .expect("provider completion items")
        .iter()
        .find(|item| item["label"] == "ProviderDocumented")
        .cloned()
        .expect("provider documented item");

    write_file(
        &provider_path,
        "unit Provider;\ninterface\n/// <summary>Changed provider documentation.</summary>\nfunction ProviderDocumented: Integer;\nimplementation\nfunction ProviderDocumented: Integer;\nbegin\n  Result := 2;\nend;\nend.\n",
    );
    let resolve_id = RequestId::from("provider-completion-resolve".to_string());
    server.send_request(resolve_id.clone(), "completionItem/resolve", item);
    let response = server.response(&resolve_id);
    let error = response
        .error
        .expect("provider disk change must invalidate resolution");
    assert_eq!(error.code, -32803);
    server.shutdown();
}

#[test]
fn completion_resolution_defers_only_the_negotiated_field_in_plaintext() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let source_path = temp.path().join("PlainCompletion.pas");
    let source = "unit PlainCompletion;\ninterface\n/// <summary>Plain documentation.</summary>\nfunction PlainDocumented: Integer;\nimplementation\nfunction PlainDocumented: Integer;\nbegin\n  Result := 1;\nend;\nprocedure Caller;\nbegin\n  PlainDoc\nend;\nend.\n";
    write_file(&source_path, source);

    let mut server = TestServer::launch();
    server.initialize_with_completion_resolve_properties(
        temp.path(),
        json!(["documentation"]),
        json!(["plaintext"]),
    );
    let completion_id = RequestId::from("plain-completion".to_string());
    server.send_request(
        completion_id.clone(),
        "textDocument/completion",
        json!({
            "textDocument": {"uri": uri(&source_path)},
            "position": position_after(source, "  PlainDoc", 0)
        }),
    );
    let initial = server
        .response(&completion_id)
        .result
        .expect("plain completion result");
    let item = initial["items"]
        .as_array()
        .expect("plain completion items")
        .iter()
        .find(|item| item["label"] == "PlainDocumented")
        .cloned()
        .expect("plain documented item");
    assert!(item["documentation"].is_null());
    let eager_detail = item["detail"]
        .as_str()
        .expect("eager plain detail")
        .to_owned();

    let resolve_id = RequestId::from("plain-completion-resolve".to_string());
    server.send_request(resolve_id.clone(), "completionItem/resolve", item);
    let response = server.response(&resolve_id);
    assert!(
        response.error.is_none(),
        "plain resolve failed: {response:?}"
    );
    let resolved = response.result.expect("plain resolved item");
    assert!(resolved["documentation"].is_string());
    assert_eq!(resolved["detail"].as_str(), Some(eager_detail.as_str()));
    server.shutdown();
}

#[test]
fn completion_resolution_preserves_generic_member_specialization() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let provider_path = temp.path().join("GenericProvider.pas");
    let main_path = temp.path().join("GenericMain.pas");
    let provider_source = "unit GenericProvider;\ninterface\ntype\n  TBox<T> = class\n    Value: T;\n  end;\nimplementation\nend.\n";
    let main_source = "unit GenericMain;\ninterface\nuses GenericProvider;\nimplementation\nprocedure Caller;\nvar\n  Box: TBox<Integer>;\nbegin\n  Box.Va\nend;\nend.\n";
    write_file(&provider_path, provider_source);
    write_file(&main_path, main_source);

    let mut server = TestServer::launch();
    server.initialize_with_completion_resolve_properties(
        temp.path(),
        json!(["detail"]),
        json!(["markdown"]),
    );
    let completion_id = RequestId::from("generic-completion".to_string());
    server.send_request(
        completion_id.clone(),
        "textDocument/completion",
        json!({
            "textDocument": {"uri": uri(&main_path)},
            "position": position_after(main_source, "  Box.Va", 0)
        }),
    );
    let initial = server
        .response(&completion_id)
        .result
        .expect("generic completion result");
    let item = initial["items"]
        .as_array()
        .expect("generic completion items")
        .iter()
        .find(|item| item["label"] == "Value")
        .cloned()
        .expect("generic Value item");
    assert!(item["detail"].is_null());

    let resolve_id = RequestId::from("generic-completion-resolve".to_string());
    server.send_request(resolve_id.clone(), "completionItem/resolve", item);
    let response = server.response(&resolve_id);
    assert!(
        response.error.is_none(),
        "generic resolve failed: {response:?}"
    );
    let resolved = response.result.expect("generic resolved item");
    assert!(
        resolved["detail"]
            .as_str()
            .is_some_and(|detail| detail.contains("Value: Integer"))
    );
    server.shutdown();
}

#[test]
fn completion_snippets_use_generic_member_signatures() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let provider_path = temp.path().join("GenericSnippetProvider.pas");
    let main_path = temp.path().join("GenericSnippetMain.pas");
    let provider_source = "unit GenericSnippetProvider;\ninterface\ntype\n  TBox<T> = class\n    procedure Put(Value: T);\n  end;\nimplementation\nprocedure TBox<T>.Put(Value: T);\nbegin\nend;\nend.\n";
    let main_source = "unit GenericSnippetMain;\ninterface\nuses GenericSnippetProvider;\nimplementation\nprocedure Caller;\nvar\n  Box: TBox<Integer>;\nbegin\n  Box.Pu;\nend;\nend.\n";
    write_file(&provider_path, provider_source);
    write_file(&main_path, main_source);

    let mut server = TestServer::launch();
    server.initialize_with_completion_capabilities(
        temp.path(),
        Some(true),
        json!([]),
        json!(["plaintext"]),
    );
    let request_id = RequestId::from("generic-member-snippet".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/completion",
        json!({
            "textDocument": {"uri": uri(&main_path)},
            "position": position_after(main_source, "  Box.Pu", 0)
        }),
    );
    let response = server.response(&request_id);
    assert!(response.error.is_none(), "completion failed: {response:?}");
    let item = response.result.expect("completion result")["items"]
        .as_array()
        .expect("completion items")
        .iter()
        .find(|item| item["label"] == "Put")
        .cloned()
        .expect("Put completion item");
    assert_eq!(item["textEdit"]["newText"], "Put(${1:Value})$0");
    assert_eq!(item["insertTextFormat"], 2);
    server.shutdown();
}

#[test]
fn completion_snippets_stay_plain_when_generic_suffix_is_already_typed() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let source_path = temp.path().join("GenericSuffixSnippet.pas");
    let source = "unit GenericSuffixSnippet;\ninterface\ntype\n  TFactory = class\n    function Make<T>: T;\n  end;\nimplementation\nfunction TFactory.Make<T>: T;\nbegin\nend;\nprocedure Caller;\nvar\n  Factory: TFactory;\nbegin\n  Factory.Ma<Integer>;\n  Factory.Ma;\nend;\nend.\n";
    write_file(&source_path, source);

    let mut server = TestServer::launch();
    server.initialize_with_completion_capabilities(
        temp.path(),
        Some(true),
        json!([]),
        json!(["plaintext"]),
    );
    let request = |server: &mut TestServer, id: &str, occurrence: usize| {
        let request_id = RequestId::from(id.to_owned());
        server.send_request(
            request_id.clone(),
            "textDocument/completion",
            json!({
                "textDocument": {"uri": uri(&source_path)},
                "position": position_after(source, "Factory.Ma", occurrence),
            }),
        );
        let response = server.response(&request_id);
        assert!(response.error.is_none(), "completion failed: {response:?}");
        let result = response.result.expect("completion result");
        result["items"]
            .as_array()
            .expect("completion items")
            .iter()
            .find(|item| item["label"] == "Make")
            .cloned()
            .unwrap_or_else(|| panic!("Make completion item missing: {result:?}"))
    };

    for (id, occurrence) in [("generic-suffix", 1), ("generic-unresolved", 2)] {
        let item = request(&mut server, id, occurrence);
        assert_eq!(item["textEdit"]["newText"], "Make");
        assert!(item["insertTextFormat"].is_null());
    }

    let suffix_item = request(&mut server, "generic-suffix-expanded", 1);
    assert_eq!(
        apply_expanded_completion_item(source, &suffix_item),
        source.replace("Factory.Ma<Integer>", "Factory.Make<Integer>")
    );
    server.shutdown();
}

#[test]
fn completion_snippets_follow_the_nearest_visible_shadowed_routine() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let source_path = temp.path().join("ShadowedRoutineSnippet.pas");
    let source = "unit ShadowedRoutineSnippet;\ninterface\nprocedure Run(Text: string);\nimplementation\nprocedure Run(Text: string);\nbegin\nend;\nprocedure Caller;\n  procedure Run(Number: Integer);\n  begin\n  end;\nbegin\n  Ru;\nend;\nend.\n";
    write_file(&source_path, source);

    let mut server = TestServer::launch();
    server.initialize_with_completion_capabilities(
        temp.path(),
        Some(true),
        json!([]),
        json!(["plaintext"]),
    );
    let request_id = RequestId::from("shadowed-routine-snippet".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/completion",
        json!({
            "textDocument": {"uri": uri(&source_path)},
            "position": position_after(source, "  Ru", 0),
        }),
    );
    let response = server.response(&request_id);
    assert!(response.error.is_none(), "completion failed: {response:?}");
    let item = response.result.expect("completion result")["items"]
        .as_array()
        .expect("completion items")
        .iter()
        .find(|item| item["label"] == "Run")
        .cloned()
        .expect("Run completion item");
    assert!(
        item["detail"]
            .as_str()
            .is_some_and(|detail| detail.contains("Number: Integer")),
        "nearest routine detail missing: {item}"
    );
    assert_eq!(item["textEdit"]["newText"], "Run(${1:Number})$0");
    assert_eq!(item["insertTextFormat"], 2);
    server.shutdown();
}

#[test]
fn completion_resolution_preserves_the_exact_overloaded_declaration() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let provider_path = temp.path().join("OverloadedProvider.pas");
    let main_path = temp.path().join("OverloadedMain.pas");
    let provider_source = "unit OverloadedProvider;\ninterface\n/// <summary>Integer overload documentation.</summary>\nfunction Pick(Value: Integer): Integer; overload;\n/// <summary>String overload documentation.</summary>\nfunction Pick(Value: string): Integer; overload;\nimplementation\nfunction Pick(Value: Integer): Integer;\nbegin\n  Result := Value;\nend;\nfunction Pick(Value: string): Integer;\nbegin\n  Result := Length(Value);\nend;\nend.\n";
    let main_source = "unit OverloadedMain;\ninterface\nuses OverloadedProvider;\nimplementation\nprocedure Caller;\nbegin\n  Pi\nend;\nend.\n";
    write_file(&provider_path, provider_source);
    write_file(&main_path, main_source);

    let mut server = TestServer::launch();
    server.initialize_with_completion_resolve_properties(
        temp.path(),
        json!(["documentation", "detail"]),
        json!(["markdown"]),
    );
    let completion_id = RequestId::from("overloaded-completion".to_string());
    server.send_request(
        completion_id.clone(),
        "textDocument/completion",
        json!({
            "textDocument": {"uri": uri(&main_path)},
            "position": position_after(main_source, "  Pi", 0)
        }),
    );
    let initial = server
        .response(&completion_id)
        .result
        .expect("overloaded completion result");
    let items = initial["items"]
        .as_array()
        .expect("overloaded completion items");
    let item = items
        .iter()
        .find(|item| item["label"] == "Pick")
        .cloned()
        .expect("overloaded Pick item");
    assert!(item["documentation"].is_null());
    assert!(item["detail"].is_null());

    let resolve_id = RequestId::from("overloaded-completion-resolve".to_string());
    server.send_request(resolve_id.clone(), "completionItem/resolve", item);
    let response = server.response(&resolve_id);
    assert!(
        response.error.is_none(),
        "overloaded resolve failed: {response:?}"
    );
    let resolved = response.result.expect("overloaded resolved item");
    assert_eq!(
        resolved["detail"],
        "function Pick(Value: Integer): Integer;"
    );
    assert_eq!(
        resolved["documentation"]["value"],
        "Integer overload documentation."
    );
    server.shutdown();
}

#[test]
fn completion_resolution_preserves_the_exact_helper_declaration() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let source_path = temp.path().join("HelperCompletion.pas");
    let source = "unit HelperCompletion;\ninterface\ntype\n  TWidget = class\n  end;\n  TWidgetHelper = class helper for TWidget\n    /// <summary>Helper method documentation.</summary>\n    procedure Assist(Value: Integer);\n  end;\n\nprocedure Caller;\n\nimplementation\n\nprocedure TWidgetHelper.Assist(Value: Integer);\nbegin\nend;\n\nprocedure Caller;\nvar\n  Widget: TWidget;\nbegin\n  Widget.Assist(1);\n  Widget.\nend;\n\nend.\n";
    write_file(&source_path, source);

    let mut server = TestServer::launch();
    server.initialize_with_completion_capabilities(
        temp.path(),
        Some(true),
        json!(["documentation", "detail"]),
        json!(["markdown"]),
    );
    let completion_id = RequestId::from("helper-completion".to_string());
    server.send_request(
        completion_id.clone(),
        "textDocument/completion",
        json!({
            "textDocument": {"uri": uri(&source_path)},
            "position": position_after(source, "  Widget.", 1)
        }),
    );
    let initial = server
        .response(&completion_id)
        .result
        .expect("helper completion result");
    let item = initial["items"]
        .as_array()
        .expect("helper completion items")
        .iter()
        .find(|item| item["label"] == "Assist")
        .cloned()
        .expect("helper Assist item");
    assert!(item["documentation"].is_null());
    assert!(item["detail"].is_null());
    assert_eq!(item["textEdit"]["newText"], "Assist(${1:Value})$0");
    assert_eq!(item["insertTextFormat"], 2);

    let resolve_id = RequestId::from("helper-completion-resolve".to_string());
    server.send_request(resolve_id.clone(), "completionItem/resolve", item);
    let response = server.response(&resolve_id);
    assert!(
        response.error.is_none(),
        "helper resolve failed: {response:?}"
    );
    let resolved = response.result.expect("helper resolved item");
    assert_eq!(resolved["detail"], "procedure Assist(Value: Integer);");
    assert_eq!(
        resolved["documentation"]["value"],
        "Helper method documentation."
    );
    server.shutdown();
}

#[test]
fn completion_resolution_rejects_a_requester_overlay_changed_after_completion() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let source_path = temp.path().join("RequesterCompletion.pas");
    let source = "unit RequesterCompletion;\ninterface\n/// <summary>Requester documentation.</summary>\nfunction RequesterDocumented: Integer;\nimplementation\nfunction RequesterDocumented: Integer;\nbegin\n  Result := 1;\nend;\nprocedure Caller;\nbegin\n  RequesterDoc\nend;\nend.\n";
    let changed_source = source.replace("RequesterDoc\n", "RequesterChanged\n");
    write_file(&source_path, source);

    let mut server = TestServer::launch();
    server.initialize_with_completion_resolve_properties(
        temp.path(),
        json!(["documentation", "detail"]),
        json!(["markdown"]),
    );
    let completion_id = RequestId::from("requester-completion".to_string());
    server.send_request(
        completion_id.clone(),
        "textDocument/completion",
        json!({
            "textDocument": {"uri": uri(&source_path)},
            "position": position_after(source, "  RequesterDoc", 0)
        }),
    );
    let initial = server
        .response(&completion_id)
        .result
        .expect("requester completion result");
    let item = initial["items"]
        .as_array()
        .expect("requester completion items")
        .iter()
        .find(|item| item["label"] == "RequesterDocumented")
        .cloned()
        .expect("requester documented item");

    server.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri(&source_path),
                "languageId": "pascal",
                "version": 2,
                "text": changed_source
            }
        }),
    );
    let resolve_id = RequestId::from("requester-completion-resolve".to_string());
    server.send_request(resolve_id.clone(), "completionItem/resolve", item);
    let response = server.response(&resolve_id);
    assert!(
        response.error.is_some(),
        "requester overlay change must invalidate resolution: {response:?}"
    );
    assert_eq!(response.error.expect("requester stale error").code, -32803);
    server.shutdown();
}

#[test]
fn completion_resolution_rejects_a_provider_overlay_change() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let main_path = temp.path().join("OverlayMain.pas");
    let provider_path = temp.path().join("OverlayProvider.pas");
    let main_source = "unit OverlayMain;\ninterface\nuses OverlayProvider;\nimplementation\nprocedure Caller;\nbegin\n  OverlayDoc\nend;\nend.\n";
    let provider_source = "unit OverlayProvider;\ninterface\n/// <summary>Original provider documentation.</summary>\nfunction OverlayDocumented: Integer;\nimplementation\nfunction OverlayDocumented: Integer;\nbegin\n  Result := 1;\nend;\nend.\n";
    let changed_provider = provider_source.replace(
        "Original provider documentation",
        "Changed provider documentation",
    );
    write_file(&main_path, main_source);
    write_file(&provider_path, provider_source);

    let mut server = TestServer::launch();
    server.initialize_with_completion_resolve_properties(
        temp.path(),
        json!(["documentation", "detail"]),
        json!(["markdown"]),
    );
    server.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri(&provider_path),
                "languageId": "pascal",
                "version": 1,
                "text": provider_source
            }
        }),
    );
    let completion_id = RequestId::from("provider-overlay-completion".to_string());
    server.send_request(
        completion_id.clone(),
        "textDocument/completion",
        json!({
            "textDocument": {"uri": uri(&main_path)},
            "position": position_after(main_source, "  OverlayDoc", 0)
        }),
    );
    let initial = server
        .response(&completion_id)
        .result
        .expect("provider overlay completion result");
    let item = initial["items"]
        .as_array()
        .expect("provider overlay completion items")
        .iter()
        .find(|item| item["label"] == "OverlayDocumented")
        .cloned()
        .expect("provider overlay completion item");

    server.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri(&provider_path),
                "languageId": "pascal",
                "version": 2,
                "text": changed_provider
            }
        }),
    );
    let resolve_id = RequestId::from("provider-overlay-completion-resolve".to_string());
    server.send_request(resolve_id.clone(), "completionItem/resolve", item);
    let response = server.response(&resolve_id);
    let error = response
        .error
        .expect("provider overlay change must invalidate resolution");
    assert_eq!(error.code, -32803);
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn completion_resolution_cancellation_returns_once_while_worker_is_in_flight() {
    let environment = tempfile::tempdir().expect("isolated server environment");
    let root = environment.path().join("workspace");
    let source_path = root.join("CancelledCompletion.pas");
    let source = "unit CancelledCompletion;\ninterface\n/// <summary>Cancellation documentation.</summary>\nfunction CancelledDocumented: Integer;\nimplementation\nfunction CancelledDocumented: Integer;\nbegin\n  Result := 1;\nend;\nprocedure Caller;\nbegin\n  CancelledDoc\nend;\nend.\n";
    write_file(&source_path, source);

    let (mut server, barrier) = TestServer::launch_with_completion_resolution_barrier(environment);
    server.initialize_with_completion_resolve_properties(
        &root,
        json!(["documentation", "detail"]),
        json!(["markdown"]),
    );
    let completion_id = RequestId::from("cancelled-completion".to_string());
    server.send_request(
        completion_id.clone(),
        "textDocument/completion",
        json!({
            "textDocument": {"uri": uri(&source_path)},
            "position": position_after(source, "  CancelledDoc", 0)
        }),
    );
    let initial = server
        .response(&completion_id)
        .result
        .expect("cancelled completion result");
    let item = initial["items"]
        .as_array()
        .expect("cancelled completion items")
        .iter()
        .find(|item| item["label"] == "CancelledDocumented")
        .cloned()
        .expect("cancelled documented item");

    let resolve_id = RequestId::from("cancelled-completion-resolve".to_string());
    server.send_request(resolve_id.clone(), "completionItem/resolve", item);
    barrier.wait_until_entered();
    server.send_notification("$/cancelRequest", json!({"id": resolve_id.clone()}));
    let response = server.response(&resolve_id);
    let error = response.error.expect("cancelled resolve error");
    assert_eq!(error.code, -32800);
    assert_eq!(error.message, "request cancelled");
    barrier.release();
    server.assert_no_response(&resolve_id);
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn completion_resolution_rejects_stale_non_cancelled_worker_delivery() {
    let environment = tempfile::tempdir().expect("isolated server environment");
    let root = environment.path().join("workspace");
    let source_path = root.join("StaleCompletion.pas");
    let source = "unit StaleCompletion;\ninterface\n/// <summary>Original documentation.</summary>\nfunction OriginalDocumented: Integer;\nimplementation\nfunction OriginalDocumented: Integer;\nbegin\n  Result := 1;\nend;\nprocedure Caller;\nbegin\n  OriginalDoc\nend;\nend.\n";
    let changed_source = source.replace("OriginalDoc", "ChangedDoc");
    write_file(&source_path, source);

    let (mut server, barrier) = TestServer::launch_with_completion_resolution_barrier(environment);
    server.initialize_with_completion_resolve_properties(
        &root,
        json!(["documentation", "detail"]),
        json!(["markdown"]),
    );
    let completion_id = RequestId::from("stale-completion".to_string());
    server.send_request(
        completion_id.clone(),
        "textDocument/completion",
        json!({
            "textDocument": {"uri": uri(&source_path)},
            "position": position_after(source, "  OriginalDoc", 0)
        }),
    );
    let initial = server
        .response(&completion_id)
        .result
        .expect("stale completion result");
    let item = initial["items"]
        .as_array()
        .expect("stale completion items")
        .iter()
        .find(|item| item["label"] == "OriginalDocumented")
        .cloned()
        .expect("stale completion item");

    let resolve_id = RequestId::from("stale-completion-resolve".to_string());
    server.send_request(resolve_id.clone(), "completionItem/resolve", item);
    barrier.wait_until_entered();
    server.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri(&source_path),
                "languageId": "pascal",
                "version": 2,
                "text": changed_source
            }
        }),
    );
    barrier.release();
    let response = server.response(&resolve_id);
    let error = response
        .error
        .expect("a changed workspace must reject the non-cancelled worker result");
    assert_eq!(error.code, -32803);
    assert_eq!(
        error.message,
        "analysis result became stale; retry the request"
    );
    server.shutdown();
}

#[test]
fn completion_request_returns_semantic_items_and_plain_text_edits() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let provider_path = temp.path().join("Provider.pas");
    let main_path = temp.path().join("Main.pas");
    let provider_source = "unit Provider;\ninterface\ntype\n  TWidget = class\n  private\n    Hidden: Integer;\n  public\n    Member: Integer;\n  end;\nprocedure PublicRoutine;\nimplementation\nprocedure PublicRoutine;\nbegin\nend;\nend.\n";
    let main_source = "unit Main;\ninterface\nuses Provider;\nimplementation\nprocedure Run;\nvar\n  LocalName: Integer;\n  Obj: TWidget;\nbegin\n  Loc;\n  Obj.Me;\nend;\nend.\n";
    write_file(&provider_path, provider_source);
    write_file(&main_path, main_source);

    let mut server = TestServer::launch();
    server.initialize(temp.path(), Value::Null);
    let id = RequestId::from("completion-request".to_string());
    server.send_request(
        id.clone(),
        "textDocument/completion",
        json!({
            "textDocument": {"uri": uri(&main_path)},
            "position": position_after(main_source, "Loc", 0),
        }),
    );
    let response = server.response(&id);
    assert!(response.error.is_none(), "completion failed: {response:?}");
    let result = response.result.expect("completion result");
    assert_eq!(result["isIncomplete"], false);
    let items = result["items"].as_array().expect("completion items");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["label"], "LocalName");
    assert_eq!(items[0]["textEdit"]["newText"], "LocalName");
    assert_eq!(
        items[0]["textEdit"]["range"],
        json!({
            "start": {"line": 6, "character": 2},
            "end": {"line": 6, "character": 11},
        })
    );
    server.shutdown();
}

#[test]
fn completion_auto_imports_an_interface_symbol_with_crlf_and_non_bmp_source() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let provider_path = temp.path().join("AutoImportProvider.pas");
    let main_path = temp.path().join("AutoImportInterface.pas");
    let provider_source = concat!(
        "unit AutoImportProvider;\r\n",
        "interface\r\n",
        "type\r\n",
        "  TImportedType = class\r\n",
        "  end;\r\n",
        "implementation\r\n",
        "end.\r\n",
    );
    let main_source = concat!(
        "unit AutoImportInterface;\r\n",
        "interface\r\n",
        "// 😀 keep this comment\r\n",
        "type\r\n",
        "  TConsumer = class\r\n",
        "    procedure Use(Value: TImportedType);\r\n",
        "  end;\r\n",
        "implementation\r\n",
        "procedure TConsumer.Use(Value: TImportedType);\r\n",
        "begin\r\n",
        "  Value := Value;\r\n",
        "end;\r\n",
        "procedure Probe;\r\n",
        "var\r\n",
        "  ImportedValue: TImportedType;\r\n",
        "begin\r\n",
        "  ImportedValue := nil;\r\n",
        "end;\r\n",
        "end.\r\n",
    );
    write_file(&provider_path, provider_source);
    write_file(&main_path, main_source);

    let mut server = TestServer::launch();
    server.initialize_with_completion_resolve_properties(
        temp.path(),
        json!(["documentation", "detail"]),
        json!(["markdown"]),
    );
    let request_id = RequestId::from("auto-import-interface".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/completion",
        json!({
            "textDocument": {"uri": uri(&main_path)},
            "position": position_after(main_source, "TImported", 0),
        }),
    );
    let response = server.response(&request_id);
    assert!(response.error.is_none(), "completion failed: {response:?}");
    let result = response.result.expect("completion result");
    let item = result["items"]
        .as_array()
        .expect("completion items")
        .iter()
        .find(|item| item["label"] == "TImportedType")
        .cloned()
        .expect("unimported interface type completion item");
    assert!(
        item["detail"]
            .as_str()
            .is_some_and(|detail| detail.contains("AutoImportProvider")),
        "auto-import item must identify its provider: {item}"
    );
    let additional = item["additionalTextEdits"]
        .as_array()
        .expect("auto-import additional edits");
    assert_eq!(additional.len(), 1);
    assert_eq!(additional[0]["newText"], "uses AutoImportProvider;\r\n");
    assert_eq!(
        additional[0]["range"],
        json!({
            "start": {"line": 2, "character": 0},
            "end": {"line": 2, "character": 0},
        })
    );
    let original_edit = item["textEdit"].clone();
    let original_additional = item["additionalTextEdits"].clone();
    let resolve_id = RequestId::from("auto-import-interface-resolve".to_string());
    server.send_request(resolve_id.clone(), "completionItem/resolve", item.clone());
    let resolved = server.response(&resolve_id);
    assert!(
        resolved.error.is_none(),
        "auto-import completion resolve failed: {resolved:?}"
    );
    let resolved = resolved.result.expect("resolved auto-import item");
    assert_eq!(resolved["textEdit"], original_edit);
    assert_eq!(resolved["additionalTextEdits"], original_additional);
    let applied = apply_completion_item(main_source, &item);
    assert!(applied.contains("interface\r\nuses AutoImportProvider;\r\n// 😀 keep this comment"));
    assert!(applied.contains("Value: TImportedType"));
    server.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri(&main_path),
                "languageId": "pascal",
                "version": 2,
                "text": applied
            }
        }),
    );
    let definition_id = RequestId::from("auto-import-interface-definition".to_string());
    server.send_request(
        definition_id.clone(),
        "textDocument/typeDefinition",
        navigation_params(&main_path, &applied, "ImportedValue :=", 0),
    );
    let locations = result_locations(server.response(&definition_id));
    assert_eq!(
        locations.len(),
        1,
        "applied source binding locations: {locations:?}\n{applied}"
    );
    assert_eq!(locations[0]["uri"], uri(&provider_path).to_string());
    server.shutdown();
}

#[test]
fn completion_auto_import_routine_snippet_keeps_its_uses_edit_on_resolve() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let provider_path = temp.path().join("AutoImportRoutineProvider.pas");
    let main_path = temp.path().join("AutoImportRoutineConsumer.pas");
    let provider_source = "unit AutoImportRoutineProvider;\ninterface\nprocedure Execute(Value: Integer);\nimplementation\nprocedure Execute(Value: Integer);\nbegin\nend;\nend.\n";
    let main_source = "unit AutoImportRoutineConsumer;\ninterface\nimplementation\nprocedure Caller;\nvar\n  Value: Integer;\nbegin\n  Exe\nend;\nend.\n";
    write_file(&provider_path, provider_source);
    write_file(&main_path, main_source);

    let mut server = TestServer::launch();
    server.initialize_with_completion_capabilities(
        temp.path(),
        Some(true),
        json!(["detail"]),
        json!(["plaintext"]),
    );
    let request_id = RequestId::from("auto-import-routine-snippet".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/completion",
        json!({
            "textDocument": {"uri": uri(&main_path)},
            "position": position_after(main_source, "  Exe", 0),
        }),
    );
    let response = server.response(&request_id);
    assert!(response.error.is_none(), "completion failed: {response:?}");
    let item = response.result.expect("completion result")["items"]
        .as_array()
        .expect("completion items")
        .iter()
        .find(|item| item["label"] == "Execute")
        .cloned()
        .expect("auto-import routine completion item");
    assert_eq!(item["textEdit"]["newText"], "Execute(${1:Value})$0");
    assert_eq!(item["insertTextFormat"], 2);
    let original_edit = item["textEdit"].clone();
    let original_additional = item["additionalTextEdits"].clone();
    assert_eq!(original_additional.as_array().map(Vec::len), Some(1));

    let mut resolve_item = item.clone();
    resolve_item["textEdit"]["newText"] = json!("FORGED_EDIT");
    resolve_item["additionalTextEdits"] = json!([]);
    let resolve_id = RequestId::from("auto-import-routine-snippet-resolve".to_string());
    server.send_request(resolve_id.clone(), "completionItem/resolve", resolve_item);
    let resolved = server.response(&resolve_id);
    assert!(
        resolved.error.is_none(),
        "auto-import routine resolve failed: {resolved:?}"
    );
    let resolved = resolved.result.expect("resolved auto-import routine item");
    assert_eq!(resolved["textEdit"], original_edit);
    assert_eq!(resolved["textEdit"]["newText"], "Execute(${1:Value})$0");
    assert_eq!(resolved["insertTextFormat"], 2);
    assert_eq!(resolved["additionalTextEdits"], original_additional);

    let applied = apply_expanded_completion_item(main_source, &item);
    assert!(applied.contains("implementation\nuses AutoImportRoutineProvider;\n"));
    assert!(applied.contains("  Execute(Value)\n"));
    assert!(!applied.contains("${1:Value}"));
    server.shutdown();
}

#[test]
fn completion_auto_imports_an_implementation_symbol_after_existing_interface_uses() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let existing_path = temp.path().join("ExistingUnit.pas");
    let provider_path = temp.path().join("AutoImportProvider.pas");
    let main_path = temp.path().join("AutoImportImplementation.pas");
    let existing_source = "unit ExistingUnit;\r\ninterface\r\nimplementation\r\nend.\r\n";
    let provider_source = concat!(
        "unit AutoImportProvider;\r\n",
        "interface\r\n",
        "type\r\n",
        "  TImportedType = class\r\n",
        "  end;\r\n",
        "implementation\r\n",
        "end.\r\n",
    );
    let main_source = concat!(
        "unit AutoImportImplementation;\r\n",
        "interface\r\n",
        "uses\r\n",
        "  ExistingUnit in 'ExistingUnit.pas'; // preserve this comment\r\n",
        "implementation\r\n",
        "// implementation comment\r\n",
        "procedure Run;\r\n",
        "var\r\n",
        "  Value: TImported;\r\n",
        "begin\r\n",
        "  Value := Value;\r\n",
        "end;\r\n",
        "end.\r\n",
    );
    write_file(&existing_path, existing_source);
    write_file(&provider_path, provider_source);
    write_file(&main_path, main_source);

    let mut server = TestServer::launch();
    server.initialize(temp.path(), Value::Null);
    let request_id = RequestId::from("auto-import-implementation".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/completion",
        json!({
            "textDocument": {"uri": uri(&main_path)},
            "position": position_after(main_source, "TImported", 0),
        }),
    );
    let response = server.response(&request_id);
    assert!(response.error.is_none(), "completion failed: {response:?}");
    let result = response.result.expect("completion result");
    let item = result["items"]
        .as_array()
        .expect("completion items")
        .iter()
        .find(|item| item["label"] == "TImportedType")
        .cloned()
        .expect("unimported implementation procedure completion item");
    assert!(
        item["detail"]
            .as_str()
            .is_some_and(|detail| detail.contains("AutoImportProvider")),
        "auto-import item must identify its provider: {item}"
    );
    let additional = item["additionalTextEdits"]
        .as_array()
        .expect("auto-import additional edits");
    assert_eq!(additional.len(), 1);
    assert_eq!(additional[0]["newText"], "uses AutoImportProvider;\r\n");
    assert_eq!(
        additional[0]["range"],
        json!({
            "start": {"line": 5, "character": 0},
            "end": {"line": 5, "character": 0},
        })
    );
    let applied = apply_completion_item(main_source, &item);
    assert!(
        applied.contains("implementation\r\nuses AutoImportProvider;\r\n// implementation comment")
    );
    assert!(applied.contains("ExistingUnit in 'ExistingUnit.pas'; // preserve this comment"));
    server.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri(&main_path),
                "languageId": "pascal",
                "version": 2,
                "text": applied,
            }
        }),
    );
    let definition_id = RequestId::from("auto-import-implementation-definition".to_string());
    server.send_request(
        definition_id.clone(),
        "textDocument/typeDefinition",
        navigation_params(&main_path, &applied, "Value :=", 0),
    );
    let locations = result_locations(server.response(&definition_id));
    assert_eq!(
        locations.len(),
        1,
        "applied implementation binding: {locations:?}"
    );
    assert_eq!(locations[0]["uri"], uri(&provider_path).to_string());
    server.shutdown();
}

#[test]
fn completion_auto_import_appends_to_an_existing_implementation_uses_clause() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let existing_path = temp.path().join("ExistingUnit.pas");
    let provider_path = temp.path().join("Append").join("Provider.pas");
    let main_path = temp.path().join("AppendConsumer.pas");
    write_file(
        &existing_path,
        "unit ExistingUnit;\r\ninterface\r\nimplementation\r\nend.\r\n",
    );
    write_file(
        &provider_path,
        concat!(
            "unit Append.Provider;\r\n",
            "interface\r\n",
            "type\r\n",
            "  TAppendedType = class\r\n",
            "  end;\r\n",
            "implementation\r\n",
            "end.\r\n",
        ),
    );
    let main_source = concat!(
        "unit AppendConsumer;\r\n",
        "interface\r\n",
        "implementation\r\n",
        "uses\r\n",
        "  ExistingUnit in 'ExistingUnit.pas'; // keep implementation comment\r\n",
        "procedure Run;\r\n",
        "var\r\n",
        "  Value: TAppended;\r\n",
        "begin\r\n",
        "  Value := Value;\r\n",
        "end;\r\n",
        "end.\r\n",
    );
    write_file(&main_path, main_source);

    let mut server = TestServer::launch();
    server.initialize(temp.path(), Value::Null);
    let request_id = RequestId::from("auto-import-append".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/completion",
        json!({
            "textDocument": {"uri": uri(&main_path)},
            "position": position_after(main_source, "TAppended", 0),
        }),
    );
    let response = server.response(&request_id);
    assert!(response.error.is_none(), "completion failed: {response:?}");
    let item = response.result.expect("completion result")["items"]
        .as_array()
        .expect("completion items")
        .iter()
        .find(|item| item["label"] == "TAppendedType")
        .cloned()
        .expect("appended auto-import item");
    assert_eq!(
        item["additionalTextEdits"][0]["newText"],
        ",\r\n  Append.Provider"
    );
    assert_eq!(
        item["additionalTextEdits"][0]["range"],
        json!({
            "start": {"line": 4, "character": 36},
            "end": {"line": 4, "character": 36},
        })
    );
    let applied = apply_completion_item(main_source, &item);
    assert!(applied.contains(
        "  ExistingUnit in 'ExistingUnit.pas',\r\n  Append.Provider; // keep implementation comment"
    ));
    server.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri(&main_path),
                "languageId": "pascal",
                "version": 2,
                "text": applied,
            }
        }),
    );
    let definition_id = RequestId::from("auto-import-append-definition".to_string());
    server.send_request(
        definition_id.clone(),
        "textDocument/typeDefinition",
        navigation_params(&main_path, &applied, "Value :=", 0),
    );
    let locations = result_locations(server.response(&definition_id));
    assert_eq!(
        locations.len(),
        1,
        "appended implementation binding: {locations:?}"
    );
    assert_eq!(locations[0]["uri"], uri(&provider_path).to_string());
    server.shutdown();
}

#[test]
fn completion_auto_import_omits_ambiguous_private_and_conditional_providers() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    write_file(
        &temp.path().join("ConditionalProvider.pas"),
        concat!(
            "unit ConditionalProvider;\n",
            "interface\n",
            "{$IFDEF NEVER_DEFINED}\n",
            "type\n",
            "  TConditionalType = class\n",
            "  end;\n",
            "{$ENDIF}\n",
            "implementation\n",
            "end.\n",
        ),
    );
    write_file(
        &temp.path().join("PrivateProvider.pas"),
        concat!(
            "unit PrivateProvider;\n",
            "interface\n",
            "implementation\n",
            "procedure HiddenProcedure;\n",
            "begin\n",
            "end;\n",
            "end.\n",
        ),
    );
    write_file(
        &temp.path().join("DuplicateProviderA.pas"),
        concat!(
            "unit DuplicateProvider;\n",
            "interface\n",
            "type\n",
            "  TDuplicateType = class\n",
            "  end;\n",
            "implementation\n",
            "end.\n",
        ),
    );
    write_file(
        &temp.path().join("DuplicateProviderB.pas"),
        concat!(
            "unit DuplicateProvider;\n",
            "interface\n",
            "type\n",
            "  TOtherType = class\n",
            "  end;\n",
            "implementation\n",
            "end.\n",
        ),
    );
    for (file_name, unit_name) in [
        ("SharedProviderA.pas", "SharedProviderA"),
        ("SharedProviderB.pas", "SharedProviderB"),
    ] {
        write_file(
            &temp.path().join(file_name),
            &format!(
                "unit {unit_name};\ninterface\ntype\n  TSharedType = class\n  end;\nimplementation\nend.\n"
            ),
        );
    }
    write_file(
        &temp.path().join("ShadowProvider.pas"),
        "unit ShadowProvider;\ninterface\ntype\n  TShadowType = class\n  end;\nimplementation\nend.\n",
    );
    let main_path = temp.path().join("NegativeAutoImportConsumer.pas");
    let main_source = concat!(
        "unit NegativeAutoImportConsumer;\n",
        "interface\n",
        "implementation\n",
        "procedure ConditionalCase;\n",
        "var\n",
        "  Value: TConditional;\n",
        "begin\n",
        "  Value := Value;\n",
        "end;\n",
        "procedure PrivateCase;\n",
        "begin\n",
        "  HiddenProc;\n",
        "end;\n",
        "procedure DuplicateCase;\n",
        "var\n",
        "  Value: TDuplicate;\n",
        "begin\n",
        "  Value := Value;\n",
        "end;\n",
        "procedure SharedCase;\n",
        "var\n",
        "  Value: TShared;\n",
        "begin\n",
        "  Value := Value;\n",
        "end;\n",
        "procedure ShadowCase;\n",
        "var\n",
        "  TShadowType: Integer;\n",
        "begin\n",
        "  TShadow;\n",
        "end;\n",
        "end.\n",
    );
    write_file(&main_path, main_source);

    let mut server = TestServer::launch();
    server.initialize(temp.path(), Value::Null);
    let completion =
        |server: &mut TestServer, id: &str, needle: &str, occurrence: usize| -> Value {
            let request_id = RequestId::from(id.to_owned());
            server.send_request(
                request_id.clone(),
                "textDocument/completion",
                json!({
                    "textDocument": {"uri": uri(&main_path)},
                    "position": position_after(main_source, needle, occurrence),
                }),
            );
            let response = server.response(&request_id);
            assert!(response.error.is_none(), "completion failed: {response:?}");
            response.result.expect("completion result")
        };
    let conditional = completion(&mut server, "auto-negative-conditional", "TConditional", 0);
    assert!(
        conditional["items"]
            .as_array()
            .expect("conditional completion items")
            .iter()
            .all(|item| item["label"] != "TConditionalType")
    );
    let private = completion(&mut server, "auto-negative-private", "HiddenProc", 0);
    assert!(
        private["items"]
            .as_array()
            .expect("private completion items")
            .iter()
            .all(|item| item["label"] != "HiddenProcedure")
    );
    let duplicate = completion(&mut server, "auto-negative-unit", "TDuplicate", 0);
    assert!(
        duplicate["items"]
            .as_array()
            .expect("ambiguous-unit completion items")
            .iter()
            .all(|item| item["label"] != "TDuplicateType")
    );
    let shared = completion(&mut server, "auto-negative-symbol", "TShared", 0);
    assert!(
        shared["items"]
            .as_array()
            .expect("ambiguous-symbol completion items")
            .iter()
            .all(|item| item["label"] != "TSharedType")
    );
    let shadow = completion(&mut server, "auto-negative-shadow", "TShadow", 1);
    let shadow_item = shadow["items"]
        .as_array()
        .expect("shadow completion items")
        .iter()
        .find(|item| item["label"] == "TShadowType")
        .expect("local shadow completion item");
    assert!(shadow_item["additionalTextEdits"].is_null());
    server.shutdown();
}

#[test]
fn completion_auto_import_omits_malformed_uses_and_does_not_duplicate_imports() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    for unit_name in ["ExistingUnit", "BrokenUnit"] {
        write_file(
            &temp.path().join(format!("{unit_name}.pas")),
            &format!("unit {unit_name};\ninterface\nimplementation\nend.\n"),
        );
    }
    write_file(
        &temp.path().join("MalformedProvider.pas"),
        "unit MalformedProvider;\ninterface\ntype\n  TMalformedType = class\n  end;\nimplementation\nend.\n",
    );
    write_file(
        &temp.path().join("DuplicateProvider.pas"),
        "unit DuplicateProvider;\ninterface\ntype\n  TDuplicateImported = class\n  end;\nimplementation\nend.\n",
    );
    let malformed_path = temp.path().join("MalformedUsesConsumer.pas");
    let malformed_source = concat!(
        "unit MalformedUsesConsumer;\n",
        "interface\n",
        "uses ExistingUnit BrokenUnit;\n",
        "type\n",
        "  TConsumer = class\n",
        "    Value: TMalformed;\n",
        "  end;\n",
        "implementation\n",
        "procedure Run;\n",
        "begin\n",
        "end;\n",
        "end.\n",
    );
    let duplicate_path = temp.path().join("DuplicateUsesConsumer.pas");
    let duplicate_source = concat!(
        "unit DuplicateUsesConsumer;\n",
        "interface\n",
        "uses DuplicateProvider;\n",
        "implementation\n",
        "procedure Run;\n",
        "var\n",
        "  Value: TDuplicateImported;\n",
        "begin\n",
        "  Value := Value;\n",
        "end;\n",
        "end.\n",
    );
    write_file(&malformed_path, malformed_source);
    write_file(&duplicate_path, duplicate_source);

    let mut server = TestServer::launch();
    server.initialize(temp.path(), Value::Null);
    let request_id = RequestId::from("auto-import-malformed-uses".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/completion",
        json!({
            "textDocument": {"uri": uri(&malformed_path)},
            "position": position_after(malformed_source, "TMalformed", 0),
        }),
    );
    let malformed = server.response(&request_id);
    assert!(
        malformed.error.is_none(),
        "completion failed: {malformed:?}"
    );
    let malformed_result = malformed.result.expect("malformed completion result");
    assert!(
        malformed_result["items"]
            .as_array()
            .expect("malformed completion items")
            .iter()
            .all(|item| item["label"] != "TMalformedType"),
        "malformed uses unexpectedly offered a target: {malformed_result}"
    );

    let request_id = RequestId::from("auto-import-duplicate-uses".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/completion",
        json!({
            "textDocument": {"uri": uri(&duplicate_path)},
            "position": position_after(duplicate_source, "TDuplicateImported", 0),
        }),
    );
    let duplicate = server.response(&request_id);
    assert!(
        duplicate.error.is_none(),
        "completion failed: {duplicate:?}"
    );
    let duplicate_result = duplicate.result.expect("duplicate completion result");
    let duplicate_item = duplicate_result["items"]
        .as_array()
        .expect("duplicate completion items")
        .iter()
        .find(|item| item["label"] == "TDuplicateImported")
        .expect("already imported completion item");
    assert!(duplicate_item["additionalTextEdits"].is_null());
    server.shutdown();
}

#[test]
fn completion_auto_import_resolution_rejects_a_late_ambiguous_unit_overlay() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let provider_path = temp.path().join("Provider.pas");
    let main_path = temp.path().join("LateAmbiguityConsumer.pas");
    let late_path = temp.path().join("LateProvider.pas");
    let provider_source = "unit Provider;\ninterface\ntype\n  TLateTargetType = class\n  end;\nimplementation\nend.\n";
    let main_source = concat!(
        "unit LateAmbiguityConsumer;\n",
        "interface\n",
        "implementation\n",
        "procedure Run;\n",
        "var\n",
        "  Value: TLateTarget;\n",
        "begin\n",
        "  Value := Value;\n",
        "end;\n",
        "end.\n",
    );
    write_file(&provider_path, provider_source);
    write_file(&main_path, main_source);

    let mut server = TestServer::launch();
    server.initialize_with_completion_resolve_properties(
        temp.path(),
        json!(["documentation", "detail"]),
        json!(["markdown"]),
    );
    let completion_id = RequestId::from("late-unit-ambiguity-completion".to_string());
    server.send_request(
        completion_id.clone(),
        "textDocument/completion",
        json!({
            "textDocument": {"uri": uri(&main_path)},
            "position": position_after(main_source, "TLateTarget", 0),
        }),
    );
    let initial = server
        .response(&completion_id)
        .result
        .expect("late ambiguity completion result");
    let item = initial["items"]
        .as_array()
        .expect("late ambiguity completion items")
        .iter()
        .find(|item| item["label"] == "TLateTargetType")
        .cloned()
        .expect("late ambiguity auto-import item");

    server.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri(&late_path),
                "languageId": "pascal",
                "version": 1,
                "text": "unit Provider; interface implementation end.\n"
            }
        }),
    );
    let resolve_id = RequestId::from("late-unit-ambiguity-resolve".to_string());
    server.send_request(resolve_id.clone(), "completionItem/resolve", item);
    let response = server.response(&resolve_id);
    let error = response
        .error
        .as_ref()
        .unwrap_or_else(|| panic!("late ambiguous unit must invalidate resolution: {response:?}"));
    assert_eq!(error.code, -32803);
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn completion_auto_import_resolution_rejects_provider_set_changes_during_worker() {
    let provider_source =
        "unit Provider;\ninterface\ntype TTargetType = class end;\nimplementation\nend.\n";
    let consumer_source = concat!(
        "unit Consumer;\n",
        "interface\n",
        "implementation\n",
        "procedure Run;\n",
        "var Value: TTarget;\n",
        "begin Value := nil; end;\n",
        "end.\n",
    );
    let negative_source = "unit Other;\ninterface\nimplementation\nend.\n";

    for (case_name, change, new_overlay, expected_resolve_error, expect_fresh_item) in [
        (
            "negative-unit",
            Some("unit Provider;\ninterface\nimplementation\nend.\n"),
            None,
            true,
            false,
        ),
        (
            "negative-symbol",
            Some("unit Other;\ninterface\ntype TTargetType = class end;\nimplementation\nend.\n"),
            None,
            true,
            false,
        ),
        (
            "new-overlay",
            None,
            Some(("New.pas", "unit Provider; interface implementation end.\n")),
            true,
            false,
        ),
        (
            "unrelated-overlay",
            Some("unit Other;\ninterface\nimplementation\nend.\n// unrelated\n"),
            None,
            false,
            true,
        ),
    ] {
        let environment = tempfile::tempdir().expect("isolated server environment");
        let root = environment.path().join("workspace");
        fs::create_dir_all(&root).expect("workspace root");
        let provider_path = root.join("Provider.pas");
        let consumer_path = root.join("Consumer.pas");
        let other_path = root.join("Other.pas");
        write_file(&provider_path, provider_source);
        write_file(&consumer_path, consumer_source);
        write_file(&other_path, negative_source);

        let (mut server, barrier) =
            TestServer::launch_with_completion_resolution_barrier(environment);
        server.initialize_with_completion_resolve_properties(
            &root,
            json!(["documentation", "detail"]),
            json!(["markdown"]),
        );
        server.send_notification(
            "textDocument/didOpen",
            json!({
                "textDocument": {
                    "uri": uri(&other_path),
                    "languageId": "pascal",
                    "version": 1,
                    "text": negative_source,
                }
            }),
        );
        let completion_id = RequestId::from(format!("provider-set-{case_name}-completion"));
        server.send_request(
            completion_id.clone(),
            "textDocument/completion",
            json!({
                "textDocument": {"uri": uri(&consumer_path)},
                "position": position_after(consumer_source, "TTarget", 0),
            }),
        );
        let initial = server
            .response(&completion_id)
            .result
            .expect("provider-set completion result");
        let item = initial["items"]
            .as_array()
            .expect("provider-set completion items")
            .iter()
            .find(|item| item["label"] == "TTargetType")
            .cloned()
            .unwrap_or_else(|| panic!("provider-set candidate missing in {case_name}: {initial}"));

        let resolve_id = RequestId::from(format!("provider-set-{case_name}-resolve"));
        server.send_request(resolve_id.clone(), "completionItem/resolve", item);
        barrier.wait_until_entered();
        if let Some(changed) = change {
            server.send_notification(
                "textDocument/didChange",
                json!({
                    "textDocument": {"uri": uri(&other_path), "version": 2},
                    "contentChanges": [{"text": changed}],
                }),
            );
        }
        if let Some((name, text)) = new_overlay {
            let path = root.join(name);
            server.send_notification(
                "textDocument/didOpen",
                json!({
                    "textDocument": {
                        "uri": uri(&path),
                        "languageId": "pascal",
                        "version": 1,
                        "text": text,
                    }
                }),
            );
        }
        // An unknown request is handled synchronously by the protocol loop;
        // its response establishes that the preceding overlay notification was
        // consumed while the resolve worker is paused.
        let sync_id = RequestId::from(format!("provider-set-{case_name}-sync"));
        server.send_request(sync_id.clone(), "review/sync", json!({}));
        let sync = server.response(&sync_id);
        assert!(
            sync.error.is_some(),
            "sync probe unexpectedly succeeded: {sync:?}"
        );
        barrier.release();
        let resolved = server.response(&resolve_id);
        assert_eq!(
            resolved.error.is_some(),
            expected_resolve_error,
            "resolve result for {case_name}: {resolved:?}"
        );

        let fresh_id = RequestId::from(format!("provider-set-{case_name}-fresh"));
        server.send_request(
            fresh_id.clone(),
            "textDocument/completion",
            json!({
                "textDocument": {"uri": uri(&consumer_path)},
                "position": position_after(consumer_source, "TTarget", 0),
            }),
        );
        let fresh = server
            .response(&fresh_id)
            .result
            .expect("fresh provider-set completion result");
        let has_item = fresh["items"]
            .as_array()
            .expect("fresh provider-set completion items")
            .iter()
            .any(|item| item["label"] == "TTargetType");
        assert_eq!(
            has_item, expect_fresh_item,
            "fresh result for {case_name}: {fresh}"
        );
        server.shutdown();
    }
}

#[cfg(target_os = "linux")]
#[test]
fn completion_auto_import_resolution_rejects_notification_free_negative_disk_changes() {
    let mut provider_source = String::from("unit Provider;\ninterface\ntype TTargetType = class\n");
    for index in 0..1500 {
        writeln!(provider_source, "  Field{index}: Integer;").expect("provider source formatting");
    }
    provider_source.push_str("end;\nimplementation\nend.\n");
    let consumer_sources = [
        (
            "ordinary",
            concat!(
                "unit Consumer;\n",
                "interface\n",
                "implementation\n",
                "procedure Run;\n",
                "var Value: TTarget;\n",
                "begin Value := nil; end;\n",
                "end.\n",
            ),
        ),
        (
            "line-comment-dot",
            concat!(
                "unit Consumer;\n",
                "interface\n",
                "implementation\n",
                "procedure Run;\n",
                "var Value:\n",
                "// .\n",
                "  TTarget;\n",
                "begin Value := nil; end;\n",
                "end.\n",
            ),
        ),
        (
            "brace-comment-dot",
            concat!(
                "unit Consumer;\n",
                "interface\n",
                "implementation\n",
                "procedure Run;\n",
                "var Value:\n",
                "{ .\n",
                "}\n",
                "  TTarget;\n",
                "begin Value := nil; end;\n",
                "end.\n",
            ),
        ),
        (
            "paren-comment-dot",
            concat!(
                "unit Consumer;\n",
                "interface\n",
                "implementation\n",
                "procedure Run;\n",
                "var Value:\n",
                "(* .\n",
                "*)\n",
                "  TTarget;\n",
                "begin Value := nil; end;\n",
                "end.\n",
            ),
        ),
    ];
    let negative_source = "unit AOther;\ninterface\nimplementation\nend.\n";

    for (source_case, consumer_source) in consumer_sources {
        for (case_name, changed_source) in [
            (
                "duplicate-unit",
                negative_source.replace("unit AOther;", "unit Provider;"),
            ),
            (
                "competing-symbol",
                "unit AOther;\ninterface\ntype TTargetType = class end;\nimplementation\nend.\n"
                    .to_string(),
            ),
        ] {
            let temp = tempfile::tempdir().expect("temporary workspace");
            let provider_path = temp.path().join("Provider.pas");
            let consumer_path = temp.path().join("Consumer.pas");
            let negative_path = temp.path().join("AOther.pas");
            write_file(&provider_path, &provider_source);
            write_file(&consumer_path, consumer_source);
            write_file(&negative_path, negative_source);

            let mut server = TestServer::launch();
            server.initialize_with_completion_resolve_properties(
                temp.path(),
                json!(["documentation", "detail"]),
                json!(["markdown"]),
            );
            let completion_id = RequestId::from(format!(
                "disk-negative-{source_case}-{case_name}-completion"
            ));
            server.send_request(
                completion_id.clone(),
                "textDocument/completion",
                json!({
                    "textDocument": {"uri": uri(&consumer_path)},
                    "position": position_after(consumer_source, "TTarget", 0),
                }),
            );
            let initial = server
                .response(&completion_id)
                .result
                .expect("disk-negative completion result");
            let item = initial["items"]
                .as_array()
                .expect("disk-negative completion items")
                .iter()
                .find(|item| item["label"] == "TTargetType")
                .cloned()
                .unwrap_or_else(|| {
                    panic!(
                        "disk-negative provider completion item missing for {source_case}/{case_name}: {initial}"
                    )
                });

            let watch_path = CString::new(negative_path.to_string_lossy().as_bytes())
                .expect("negative provider watch path");
            let fd = unsafe { inotify_init1(0) };
            assert!(fd >= 0, "inotify_init1 failed");
            let watch = unsafe { inotify_add_watch(fd, watch_path.as_ptr(), IN_CLOSE_NOWRITE) };
            assert!(watch >= 0, "inotify_add_watch failed");
            let changed_path = negative_path.clone();
            let (mutated_sender, mutated_receiver) = mpsc::channel();
            let watcher = thread::spawn(move || {
                wait_for_close_events(fd, 1);
                write_file(&changed_path, &changed_source);
                mutated_sender.send(()).expect("notify disk mutation");
            });

            let resolve_id =
                RequestId::from(format!("disk-negative-{source_case}-{case_name}-resolve"));
            server.send_request(resolve_id.clone(), "completionItem/resolve", item);
            mutated_receiver
                .recv_timeout(IO_TIMEOUT)
                .expect("resolve must read the negative provider before the mutation");
            let resolved = server.response(&resolve_id);
            watcher.join().expect("disk mutation watcher");
            assert_eq!(
                resolved.error.as_ref().map(|error| error.code),
                Some(-32803),
                "notification-free {source_case}/{case_name} disk mutation was accepted: {resolved:?}"
            );

            let fresh_id =
                RequestId::from(format!("disk-negative-{source_case}-{case_name}-fresh"));
            server.send_request(
                fresh_id.clone(),
                "textDocument/completion",
                json!({
                    "textDocument": {"uri": uri(&consumer_path)},
                    "position": position_after(consumer_source, "TTarget", 0),
                }),
            );
            let fresh = server
                .response(&fresh_id)
                .result
                .expect("fresh disk-negative completion result");
            assert!(
                fresh["items"]
                    .as_array()
                    .expect("fresh disk-negative completion items")
                    .iter()
                    .all(|item| item["label"] != "TTargetType"),
                "fresh {source_case}/{case_name} completion retained the ambiguous candidate: {fresh}"
            );
            server.shutdown();
        }
    }
}

#[test]
fn completion_auto_import_requires_project_unit_binding_and_preserves_namespace_binding() {
    let assert_omitted = |case_name: &str, files: &[(&str, &str)], project: Option<&str>| {
        let temp = tempfile::tempdir().expect("temporary workspace");
        for (name, source) in files {
            let path = temp.path().join(name);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).expect("fixture parent");
            }
            write_file(&path, source);
        }
        if let Some(project) = project {
            write_file(&temp.path().join("App.dproj"), project);
        }
        let main_path = temp.path().join("Consumer.pas");
        let main_source = files
            .iter()
            .find(|(name, _)| *name == "Consumer.pas")
            .map(|(_, source)| *source)
            .expect("consumer fixture");
        let mut server = TestServer::launch();
        server.initialize(temp.path(), Value::Null);
        let request_id = RequestId::from(format!("{case_name}-completion"));
        server.send_request(
            request_id.clone(),
            "textDocument/completion",
            json!({
                "textDocument": {"uri": uri(&main_path)},
                "position": position_after(main_source, "TTarget", 0),
            }),
        );
        let response = server.response(&request_id);
        assert!(
            response.error.is_none(),
            "{case_name} completion failed: {response:?}"
        );
        let result = response.result.expect("binding-proof completion result");
        assert!(
            result["items"]
                .as_array()
                .expect("binding-proof completion items")
                .iter()
                .all(|item| item["label"] != "TTargetType"),
            "unproven provider binding unexpectedly offered a candidate in {case_name}: {result}"
        );
        server.shutdown();
    };

    let consumer_source = concat!(
        "unit Consumer;\n",
        "interface\n",
        "implementation\n",
        "procedure Run;\n",
        "var Value: TTarget;\n",
        "begin Value := nil; end;\n",
        "end.\n",
    );
    let provider_source =
        "unit Provider;\ninterface\ntype TTargetType = class end;\nimplementation\nend.\n";
    let empty_other = "unit Other; interface implementation end.\n";

    assert_omitted(
        "filename-mismatch",
        &[
            ("WrongFilename.pas", provider_source),
            ("Consumer.pas", consumer_source),
        ],
        None,
    );
    assert_omitted(
        "project-subdirectory-without-search-path",
        &[
            ("sub/Provider.pas", provider_source),
            ("Consumer.pas", consumer_source),
        ],
        Some(
            "<Project><PropertyGroup><MainSource>Consumer.pas</MainSource></PropertyGroup></Project>",
        ),
    );
    assert_omitted(
        "unit-alias-redirect",
        &[
            ("Provider.pas", provider_source),
            ("Other.pas", empty_other),
            ("Consumer.pas", consumer_source),
        ],
        Some(
            "<Project><PropertyGroup><MainSource>Consumer.pas</MainSource><DCC_UnitAlias>Provider=Other</DCC_UnitAlias></PropertyGroup></Project>",
        ),
    );

    let temp = tempfile::tempdir().expect("namespace workspace");
    let namespace_provider = temp.path().join("Vendor.Provider.pas");
    let empty_provider = temp.path().join("Provider.pas");
    let main_path = temp.path().join("Consumer.pas");
    write_file(
        &namespace_provider,
        "unit Vendor.Provider;\ninterface\ntype TTargetType = class end;\nimplementation\nend.\n",
    );
    write_file(
        &empty_provider,
        empty_other.replace("Other", "Provider").as_str(),
    );
    write_file(
        &temp.path().join("App.dproj"),
        concat!(
            "<Project><PropertyGroup>",
            "<MainSource>Consumer.pas</MainSource>",
            "<DCC_Namespace>Vendor</DCC_Namespace>",
            "</PropertyGroup></Project>"
        ),
    );
    write_file(&main_path, consumer_source);
    let mut server = TestServer::launch();
    server.initialize(temp.path(), Value::Null);
    let request_id = RequestId::from("namespace-binding-completion".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/completion",
        json!({
            "textDocument": {"uri": uri(&main_path)},
            "position": position_after(consumer_source, "TTarget", 0),
        }),
    );
    let result = server
        .response(&request_id)
        .result
        .expect("namespace completion result");
    let item = result["items"]
        .as_array()
        .expect("namespace completion items")
        .iter()
        .find(|item| item["label"] == "TTargetType")
        .cloned()
        .expect("namespace provider completion item");
    assert_eq!(
        item["additionalTextEdits"][0]["newText"],
        "uses Vendor.Provider;\n"
    );
    let applied = apply_completion_item(consumer_source, &item);
    assert!(applied.contains("uses Vendor.Provider;"));
    server.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri(&main_path),
                "languageId": "pascal",
                "version": 2,
                "text": applied,
            }
        }),
    );
    let definition_id = RequestId::from("namespace-binding-definition".to_string());
    server.send_request(
        definition_id.clone(),
        "textDocument/typeDefinition",
        navigation_params(&main_path, &applied, "Value :=", 0),
    );
    let locations = result_locations(server.response(&definition_id));
    assert_eq!(
        locations.len(),
        1,
        "namespace binding locations: {locations:?}"
    );
    assert_eq!(locations[0]["uri"], uri(&namespace_provider).to_string());
    server.shutdown();
}

#[test]
fn completion_auto_import_omits_unsafe_absent_uses_in_comments_and_same_line_declarations() {
    let provider_source =
        "unit Provider;\ninterface\ntype TTargetType = class end;\nimplementation\nend.\n";
    let cases = [
        (
            "brace-comment-implementation",
            concat!(
                "unit Consumer;\ninterface\n",
                "implementation { open comment\nclosed }\n",
                "procedure Run;\nvar Value: TTarget;\nbegin Value := nil; end;\nend.\n",
            ),
            "TTarget",
        ),
        (
            "paren-comment-interface",
            concat!(
                "unit Consumer;\n",
                "interface (* open comment\nclosed *)\n",
                "type TAlias = TTarget;\nimplementation\nend.\n",
            ),
            "TTarget",
        ),
        (
            "same-line-implementation",
            concat!(
                "unit Consumer;\ninterface\n",
                "implementation procedure Run;\n",
                "var Value: TTarget;\nbegin Value := nil; end;\nend.\n",
            ),
            "TTarget",
        ),
    ];
    for (case_name, source, needle) in cases {
        let temp = tempfile::tempdir().expect("temporary workspace");
        write_file(&temp.path().join("Provider.pas"), provider_source);
        let main_path = temp.path().join("Consumer.pas");
        write_file(&main_path, source);
        let mut server = TestServer::launch();
        server.initialize(temp.path(), Value::Null);
        let request_id = RequestId::from(format!("{case_name}-completion"));
        server.send_request(
            request_id.clone(),
            "textDocument/completion",
            json!({
                "textDocument": {"uri": uri(&main_path)},
                "position": position_after(source, needle, 0),
            }),
        );
        let response = server.response(&request_id);
        assert!(
            response.error.is_none(),
            "{case_name} completion failed: {response:?}"
        );
        let result = response.result.expect("unsafe insertion completion result");
        assert!(
            result["items"]
                .as_array()
                .expect("unsafe insertion completion items")
                .iter()
                .all(|item| item["label"] != "TTargetType"),
            "unsafe absent-uses insertion unexpectedly offered a candidate in {case_name}: {result}"
        );
        server.shutdown();
    }
}

#[test]
fn completion_auto_import_omits_dangling_and_repeated_uses_alias_operators() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    write_file(
        &temp.path().join("Provider.pas"),
        "unit Provider;\ninterface\ntype TTargetType = class end;\nimplementation\nend.\n",
    );
    write_file(
        &temp.path().join("Existing.pas"),
        "unit Existing; interface implementation end.\n",
    );
    let cases = [
        ("dangling", "uses Existing := ;\n"),
        ("repeated", "uses Existing := Existing := ;\n"),
    ];
    for (case_name, uses_clause) in cases {
        let source = format!(
            "unit Consumer;\ninterface\nimplementation\n{uses_clause}procedure Run;\nvar Value: TTarget;\nbegin Value := nil; end;\nend.\n"
        );
        let main_path = temp.path().join(format!("{case_name}.pas"));
        write_file(&main_path, &source);
        let mut server = TestServer::launch();
        server.initialize(temp.path(), Value::Null);
        let request_id = RequestId::from(format!("malformed-{case_name}-completion"));
        server.send_request(
            request_id.clone(),
            "textDocument/completion",
            json!({
                "textDocument": {"uri": uri(&main_path)},
                "position": position_after(&source, "TTarget", 0),
            }),
        );
        let response = server.response(&request_id);
        if let Some(error) = response.error {
            assert!(
                error
                    .message
                    .contains("assistance dependency scan incomplete"),
                "unexpected {case_name} completion failure: {error:?}"
            );
            server.shutdown();
            continue;
        }
        let result = response.result.expect("malformed uses completion result");
        assert!(
            result["items"]
                .as_array()
                .expect("malformed uses completion items")
                .iter()
                .all(|item| item["label"] != "TTargetType"),
            "malformed uses unexpectedly offered a candidate in {case_name}: {result}"
        );
        server.shutdown();
    }
}

#[test]
fn completion_auto_import_omits_uses_clauses_enclosed_by_conditionals() {
    let temp = tempfile::tempdir().expect("conditional workspace");
    write_file(
        &temp.path().join("Provider.pas"),
        "unit Provider;\ninterface\ntype TTargetType = class end;\nimplementation\nend.\n",
    );
    write_file(
        &temp.path().join("Existing.pas"),
        "unit Existing; interface implementation end.\n",
    );
    write_file(
        &temp.path().join("App.dproj"),
        "<Project><PropertyGroup><MainSource>ImplementationConsumer.pas</MainSource><DCC_Define>FOO</DCC_Define></PropertyGroup></Project>",
    );
    let cases = [
        (
            "implementation-active",
            "ImplementationConsumer.pas",
            concat!(
                "unit ImplementationConsumer;\ninterface\nimplementation\n",
                "{$IFDEF FOO}\nuses Existing;\n{$ENDIF}\n",
                "procedure Run;\nvar Value: TTarget;\nbegin Value := nil; end;\nend.\n",
            ),
        ),
        (
            "interface-active",
            "InterfaceConsumer.pas",
            concat!(
                "unit InterfaceConsumer;\ninterface\n",
                "{$IFDEF FOO}\nuses Existing;\n{$ENDIF}\n",
                "type TAlias = TTarget;\nimplementation\nend.\n",
            ),
        ),
        (
            "implementation-nested",
            "NestedConsumer.pas",
            concat!(
                "unit NestedConsumer;\ninterface\nimplementation\n",
                "{$IFDEF FOO}\n{$IFDEF BAR}\nuses Existing;\n{$ENDIF}\n{$ENDIF}\n",
                "procedure Run;\nvar Value: TTarget;\nbegin Value := nil; end;\nend.\n",
            ),
        ),
    ];
    for (case_name, file_name, source) in cases {
        let path = temp.path().join(file_name);
        write_file(&path, source);
        let mut server = TestServer::launch();
        server.initialize(temp.path(), Value::Null);
        let request_id = RequestId::from(format!("{case_name}-completion"));
        server.send_request(
            request_id.clone(),
            "textDocument/completion",
            json!({
                "textDocument": {"uri": uri(&path)},
                "position": position_after(source, "TTarget", 0),
            }),
        );
        let response = server.response(&request_id);
        if let Some(error) = response.error {
            assert!(
                error
                    .message
                    .contains("assistance dependency scan incomplete"),
                "unexpected {case_name} completion failure: {error:?}"
            );
            server.shutdown();
            continue;
        }
        let result = response.result.expect("conditional completion result");
        assert!(
            result["items"]
                .as_array()
                .expect("conditional completion items")
                .iter()
                .all(|item| item["label"] != "TTargetType"),
            "conditionally enclosed uses unexpectedly offered a candidate in {case_name}: {result}"
        );
        server.shutdown();
    }
}

#[test]
fn completion_auto_import_uses_the_prefix_before_a_middle_of_token_caret() {
    let temp = tempfile::tempdir().expect("middle-token workspace");
    let provider_path = temp.path().join("Provider.pas");
    let main_path = temp.path().join("Consumer.pas");
    write_file(
        &provider_path,
        "unit Provider;\ninterface\ntype TTargetType = class end;\nimplementation\nend.\n",
    );
    let source = concat!(
        "unit Consumer;\n",
        "interface\n",
        "implementation\n",
        "procedure Run;\n",
        "var Value: TTargetWrong;\n",
        "begin Value := nil; end;\n",
        "end.\n",
    );
    write_file(&main_path, source);
    let mut server = TestServer::launch();
    server.initialize(temp.path(), Value::Null);
    let start = position_of(source, "TTargetWrong", 0);
    let prefix_position = Position::new(
        start.line,
        start.character + "TTarget".encode_utf16().count() as u32,
    );
    let request_id = RequestId::from("middle-token-completion".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/completion",
        json!({
            "textDocument": {"uri": uri(&main_path)},
            "position": prefix_position,
        }),
    );
    let result = server
        .response(&request_id)
        .result
        .expect("middle-token completion result");
    let item = result["items"]
        .as_array()
        .expect("middle-token completion items")
        .iter()
        .find(|item| item["label"] == "TTargetType")
        .cloned()
        .expect("middle-token auto-import item");
    let applied = apply_completion_item(source, &item);
    assert!(
        applied.contains("Value: TTargetType;"),
        "applied middle-token edit: {applied}"
    );
    server.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri(&main_path),
                "languageId": "pascal",
                "version": 2,
                "text": applied,
            }
        }),
    );
    let definition_id = RequestId::from("middle-token-definition".to_string());
    server.send_request(
        definition_id.clone(),
        "textDocument/typeDefinition",
        navigation_params(&main_path, &applied, "Value :=", 0),
    );
    let locations = result_locations(server.response(&definition_id));
    assert_eq!(
        locations.len(),
        1,
        "middle-token binding locations: {locations:?}"
    );
    assert_eq!(locations[0]["uri"], uri(&provider_path).to_string());
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn completion_auto_import_discovery_reports_a_bounded_catalogue_as_incomplete() {
    let environment = tempfile::tempdir().expect("isolated test environment");
    let root = environment.path().join("workspace");
    fs::create_dir_all(&root).expect("workspace root");
    let main_path = root.join("BoundedConsumer.pas");
    write_file(
        &root.join("BoundedProvider.pas"),
        "unit BoundedProvider;\ninterface\ntype\n  TBoundedType = class\n  end;\nimplementation\nend.\n",
    );
    let main_source = "unit BoundedConsumer;\ninterface\nimplementation\nprocedure Run;\nvar\n  Value: TBounded;\nbegin\n  Value := Value;\nend;\nend.\n";
    write_file(&main_path, main_source);

    let (mut server, barrier) =
        TestServer::launch_with_navigation_barrier_and_filename_catalogue_limit(environment, 1);
    server.initialize(&root, Value::Null);
    let request_id = RequestId::from("auto-import-bounded".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/completion",
        json!({
            "textDocument": {"uri": uri(&main_path)},
            "position": position_after(main_source, "TBounded", 0),
        }),
    );
    barrier.release();
    let response = server.response(&request_id);
    assert!(
        response.error.is_none(),
        "bounded completion failed: {response:?}"
    );
    let result = response.result.expect("bounded completion result");
    assert_eq!(result["isIncomplete"], true);
    assert!(
        result["items"]
            .as_array()
            .expect("bounded completion items")
            .iter()
            .all(|item| item["label"] != "TBoundedType")
    );
    server.shutdown();
}

#[test]
fn deep_method_receiver_completion_stays_stack_safe_and_keeps_server_responsive() {
    let temp = tempfile::tempdir().unwrap();
    let source_path = temp.path().join("DeepMethodReceivers.pas");
    let method_chain = |count: usize| {
        let mut expression = String::from("Obj");
        for _ in 0..count {
            expression.push_str(".Next()");
        }
        expression.push_str(".Me");
        expression
    };
    let chain_256 = method_chain(256);
    let chain_512 = method_chain(512);
    let source = format!(
        "unit DeepMethodReceivers;\ninterface\ntype\n  TObj = class\n    function Next: TObj;\n    Member: Integer;\n  end;\nimplementation\nfunction TObj.Next: TObj;\nbegin\n  Result := Self;\nend;\nprocedure Run256;\nvar\n  Obj: TObj;\nbegin\n  {chain_256};\nend;\nprocedure Run512;\nvar\n  Obj: TObj;\nbegin\n  {chain_512};\nend;\nend.\n"
    );
    write_file(&source_path, &source);

    let mut server = TestServer::launch();
    server.initialize(temp.path(), Value::Null);

    let outline_id = RequestId::from("deep-method-outline".to_string());
    server.send_request(
        outline_id.clone(),
        "textDocument/documentSymbol",
        json!({"textDocument": {"uri": uri(&source_path)}}),
    );
    let outline = server.response(&outline_id);
    assert!(
        outline.error.is_none(),
        "document symbols failed: {outline:?}"
    );

    for (request_name, chain) in [
        ("deep-method-completion-256", &chain_256),
        ("deep-method-completion-512", &chain_512),
    ] {
        let id = RequestId::from(request_name.to_string());
        server.send_request(
            id.clone(),
            "textDocument/completion",
            json!({
                "textDocument": {"uri": uri(&source_path)},
                "position": position_after(&source, chain, 0),
            }),
        );
        let response = server.response(&id);
        assert!(
            response.error.is_none(),
            "deep completion failed: {response:?}"
        );
        assert_eq!(
            response.result.expect("deep completion result")["isIncomplete"],
            true
        );
    }

    let cancelled_id = RequestId::from("deep-method-completion-cancelled".to_string());
    server.send_request(
        cancelled_id.clone(),
        "textDocument/completion",
        json!({
            "textDocument": {"uri": uri(&source_path)},
            "position": position_after(&source, &chain_512, 0),
        }),
    );
    server.send_notification(
        "$/cancelRequest",
        json!({"id": "deep-method-completion-cancelled"}),
    );
    let cancelled = server.response(&cancelled_id);
    let error = cancelled
        .error
        .expect("cancelled deep completion must fail");
    assert_eq!(error.code, -32800);
    assert_eq!(error.message, "request cancelled");

    let responsive_id = RequestId::from("deep-method-responsive".to_string());
    server.send_request(
        responsive_id.clone(),
        "textDocument/documentSymbol",
        json!({"textDocument": {"uri": uri(&source_path)}}),
    );
    let responsive = server.response(&responsive_id);
    assert!(
        responsive.error.is_none(),
        "server stopped responding after deep completion: {responsive:?}"
    );
    server.shutdown();
}

#[test]
fn deeply_nested_generic_receiver_completion_stays_bounded_and_server_responsive() {
    let temp = tempfile::tempdir().unwrap();
    let source_path = temp.path().join("DeepGenericReceivers.pas");
    let depth = 512;
    let mut nested_type = String::from("TWidget");
    for _ in 0..depth {
        nested_type = format!("TBox<{nested_type}>");
    }
    let mut expression = String::from("Box");
    for _ in 0..depth {
        expression.push_str(".Value");
    }
    expression.push_str(".Me");
    let source = format!(
        "unit DeepGenericReceivers;\ninterface\ntype\n  TBox<T> = class\n    Value: T;\n  end;\n  TWidget = class\n    Member: Integer;\n  end;\nimplementation\nprocedure Run;\nvar\n  Box: {nested_type};\nbegin\n  {expression};\nend;\nend.\n"
    );
    write_file(&source_path, &source);

    let mut server = TestServer::launch();
    server.initialize(temp.path(), Value::Null);

    let outline_id = RequestId::from("deep-generic-outline".to_string());
    server.send_request(
        outline_id.clone(),
        "textDocument/documentSymbol",
        json!({"textDocument": {"uri": uri(&source_path)}}),
    );
    let outline = server.response(&outline_id);
    assert!(
        outline.error.is_none(),
        "deep generic document symbols failed: {outline:?}"
    );

    let id = RequestId::from("deep-generic-completion".to_string());
    server.send_request(
        id.clone(),
        "textDocument/completion",
        json!({
            "textDocument": {"uri": uri(&source_path)},
            "position": position_after(&source, &expression, 0),
        }),
    );
    let response = server.response(&id);
    assert!(
        response.error.is_none(),
        "deep generic completion failed: {response:?}"
    );
    let result = response.result.expect("deep generic completion result");
    assert_eq!(result["items"], json!([]));

    let cancelled_id = RequestId::from("deep-generic-completion-cancelled".to_string());
    server.send_request(
        cancelled_id.clone(),
        "textDocument/completion",
        json!({
            "textDocument": {"uri": uri(&source_path)},
            "position": position_after(&source, &expression, 0),
        }),
    );
    server.send_notification(
        "$/cancelRequest",
        json!({"id": "deep-generic-completion-cancelled"}),
    );
    let cancelled = server.response(&cancelled_id);
    let error = cancelled
        .error
        .expect("cancelled deep generic completion must fail");
    assert_eq!(error.code, -32800);
    assert_eq!(error.message, "request cancelled");

    let responsive_id = RequestId::from("deep-generic-responsive".to_string());
    server.send_request(
        responsive_id.clone(),
        "textDocument/documentSymbol",
        json!({"textDocument": {"uri": uri(&source_path)}}),
    );
    let responsive = server.response(&responsive_id);
    assert!(
        responsive.error.is_none(),
        "server stopped responding after deep generic completion: {responsive:?}"
    );
    server.shutdown();
}

#[test]
fn deeply_nested_with_completion_cancels_and_keeps_server_responsive() {
    let temp = tempfile::tempdir().unwrap();
    let source_path = temp.path().join("DeepWithReceivers.pas");
    let depth = 512;
    let mut source = String::from(
        "unit DeepWithReceivers;\ninterface\ntype\n  TObj = class\n    Member: Integer;\n  end;\nimplementation\nprocedure Run;\nvar\n  Obj: TObj;\nbegin\n",
    );
    for _ in 0..depth {
        source.push_str("  with Obj do begin\n");
    }
    let cursor_line = source[..source.len()]
        .bytes()
        .filter(|byte| *byte == b'\n')
        .count() as u32;
    source.push_str("    \n");
    for _ in 0..depth {
        source.push_str("  end;\n");
    }
    source.push_str("end;\nend.\n");
    write_file(&source_path, &source);

    let mut server = TestServer::launch();
    server.initialize(temp.path(), Value::Null);
    let position = Position::new(cursor_line, 4);

    let completion_id = RequestId::from("deep-with-completion".to_string());
    server.send_request(
        completion_id.clone(),
        "textDocument/completion",
        json!({
            "textDocument": {"uri": uri(&source_path)},
            "position": position,
        }),
    );
    let completion = server.response(&completion_id);
    assert!(
        completion.error.is_none(),
        "deep with completion failed: {completion:?}"
    );
    assert_eq!(
        completion.result.expect("deep with completion result")["isIncomplete"],
        true
    );

    let cancelled_id = RequestId::from("deep-with-completion-cancelled".to_string());
    server.send_request(
        cancelled_id.clone(),
        "textDocument/completion",
        json!({
            "textDocument": {"uri": uri(&source_path)},
            "position": position,
        }),
    );
    server.send_notification(
        "$/cancelRequest",
        json!({"id": "deep-with-completion-cancelled"}),
    );
    let cancelled = server.response(&cancelled_id);
    let error = cancelled
        .error
        .expect("cancelled deep with completion must fail");
    assert_eq!(error.code, -32800);
    assert_eq!(error.message, "request cancelled");

    let responsive_id = RequestId::from("deep-with-responsive".to_string());
    server.send_request(
        responsive_id.clone(),
        "textDocument/documentSymbol",
        json!({"textDocument": {"uri": uri(&source_path)}}),
    );
    let responsive = server.response(&responsive_id);
    assert!(
        responsive.error.is_none(),
        "server stopped responding after deep with completion: {responsive:?}"
    );
    server.shutdown();
}

#[test]
fn cyclic_generic_member_types_fail_closed_and_keep_the_server_responsive() {
    let temp = tempfile::tempdir().unwrap();
    let source_path = temp.path().join("CyclicGeneric.pas");
    let source = "unit CyclicGeneric;\ninterface\ntype\n  TNode<T> = class\n    Next: TNode<T>;\n  end;\nimplementation\nprocedure Run;\nvar\n  Node: TNode<Integer>;\nbegin\n  Node.Next.Mis;\nend;\nend.\n";
    write_file(&source_path, source);

    let mut server = TestServer::launch();
    server.initialize(temp.path(), Value::Null);

    let completion_id = RequestId::from("cyclic-generic-completion".to_string());
    server.send_request(
        completion_id.clone(),
        "textDocument/completion",
        json!({
            "textDocument": {"uri": uri(&source_path)},
            "position": position_after(source, "Node.Next.Mis", 0),
        }),
    );
    let completion = server.response(&completion_id);
    assert!(
        completion.error.is_none(),
        "cyclic generic completion failed: {completion:?}"
    );
    assert_eq!(
        completion.result.expect("cyclic generic completion result")["items"],
        json!([])
    );

    let responsive_id = RequestId::from("cyclic-generic-responsive".to_string());
    server.send_request(
        responsive_id.clone(),
        "textDocument/documentSymbol",
        json!({"textDocument": {"uri": uri(&source_path)}}),
    );
    let responsive = server.response(&responsive_id);
    assert!(
        responsive.error.is_none(),
        "server stopped responding after cyclic generic completion: {responsive:?}"
    );
    server.shutdown();
}

#[test]
fn invalid_helper_ancestry_fails_closed_and_keeps_server_responsive() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let source_path = temp.path().join("InvalidHelperAncestry.pas");
    let source = "unit InvalidHelperAncestry;\ninterface\ntype\n  TBase = class\n  end;\n  TWidget = class\n  end;\n  TWidgetHelper = class helper (TBase) for TWidget\n    procedure Touch;\n  end;\nimplementation\nprocedure TWidgetHelper.Touch;\nbegin\nend;\nprocedure Run;\nvar\n  Widget: TWidget;\nbegin\n  Widget.Touch;\nend;\nend.\n";
    write_file(&source_path, source);

    let mut server = TestServer::launch();
    server.initialize(temp.path(), Value::Null);

    let definition_id = RequestId::from("invalid-helper-ancestry-definition".to_string());
    server.send_request(
        definition_id.clone(),
        "textDocument/definition",
        navigation_params(&source_path, source, "Touch", 2),
    );
    let locations = result_locations(server.response(&definition_id));
    assert!(
        locations.is_empty(),
        "invalid helper ancestry must fail closed: {locations:?}"
    );

    let responsive_id = RequestId::from("invalid-helper-ancestry-responsive".to_string());
    server.send_request(
        responsive_id.clone(),
        "textDocument/documentSymbol",
        json!({"textDocument": {"uri": uri(&source_path)}}),
    );
    let responsive = server.response(&responsive_id);
    assert!(
        responsive.error.is_none(),
        "server stopped responding after invalid helper ancestry: {responsive:?}"
    );
    server.shutdown();
}

#[test]
fn helper_owner_keeps_lexical_members_when_another_helper_is_active() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let source_path = temp.path().join("HelperOwnerLexical.pas");
    let source = "unit HelperOwnerLexical;\ninterface\ntype\n  TWidget = class\n  end;\n  TFirstHelper = class helper for TWidget\n    procedure First;\n    procedure Second;\n  end;\n  TSecondHelper = class helper for TWidget\n    procedure Other;\n  end;\nimplementation\nprocedure TFirstHelper.Second;\nbegin\nend;\nprocedure TSecondHelper.Other;\nbegin\nend;\nprocedure TFirstHelper.First;\nvar\n  Second: Integer;\nbegin\n  Second := 1;\n  Self.Second;\nend;\nend.\n";
    write_file(&source_path, source);

    let mut server = TestServer::launch();
    server.initialize(temp.path(), Value::Null);

    let local_id = RequestId::from("helper-owner-local".to_string());
    server.send_request(
        local_id.clone(),
        "textDocument/definition",
        navigation_params(&source_path, source, "Second :=", 0),
    );
    let local_locations = result_locations(server.response(&local_id));
    assert_exact_location_signatures(
        &local_locations,
        vec![expected_location_signature(
            &source_path,
            source,
            "Second",
            4,
        )],
    );

    let self_id = RequestId::from("helper-owner-self".to_string());
    server.send_request(
        self_id.clone(),
        "textDocument/definition",
        json!({
            "textDocument": {"uri": uri(&source_path)},
            "position": final_qualified_type_position(source, "Self.Second"),
        }),
    );
    let self_locations = result_locations(server.response(&self_id));
    assert_exact_location_signatures(
        &self_locations,
        vec![expected_location_signature(
            &source_path,
            source,
            "Second",
            2,
        )],
    );

    let completion_id = RequestId::from("helper-owner-completion".to_string());
    server.send_request(
        completion_id.clone(),
        "textDocument/completion",
        json!({
            "textDocument": {"uri": uri(&source_path)},
            "position": position_after(source, "Self.", 0),
        }),
    );
    let completion = server.response(&completion_id);
    assert!(
        completion.error.is_none(),
        "helper owner completion failed: {completion:?}"
    );
    let completion_result = completion.result.expect("helper owner completion result");
    let items = completion_result["items"]
        .as_array()
        .expect("helper owner completion items");
    let labels = items
        .iter()
        .filter_map(|item| item["label"].as_str())
        .collect::<HashSet<_>>();
    assert!(
        labels.contains("Second"),
        "own helper member missing: {labels:?}"
    );
    assert!(
        !labels.contains("Other"),
        "external helper leaked: {labels:?}"
    );

    server.shutdown();
}

#[test]
fn unknown_conditional_import_does_not_select_a_known_helper() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let target_path = temp.path().join("ConditionalHelperTarget.pas");
    let known_path = temp.path().join("KnownConditionalHelper.pas");
    let maybe_path = temp.path().join("MaybeConditionalHelper.pas");
    let consumer_path = temp.path().join("ConditionalHelperConsumer.pas");
    let target = "unit ConditionalHelperTarget;\ninterface\ntype\n  TWidget = class\n  end;\nimplementation\nend.\n";
    let known = "unit KnownConditionalHelper;\ninterface\nuses ConditionalHelperTarget;\ntype\n  TKnownHelper = class helper for TWidget\n    procedure Touch;\n  end;\nimplementation\nprocedure TKnownHelper.Touch;\nbegin\nend;\nend.\n";
    let maybe = "unit MaybeConditionalHelper;\ninterface\nuses ConditionalHelperTarget;\ntype\n  TMaybeHelper = class helper for TWidget\n    procedure Touch;\n  end;\nimplementation\nprocedure TMaybeHelper.Touch;\nbegin\nend;\nend.\n";
    let consumer = "unit ConditionalHelperConsumer;\ninterface\nuses\n  ConditionalHelperTarget,\n  KnownConditionalHelper,\n  {$IF CompilerVersion >= 24}\n  MaybeConditionalHelper\n  {$ENDIF};\nimplementation\nprocedure Run;\nvar\n  Widget: ConditionalHelperTarget.TWidget;\nbegin\n  Widget.Touch;\nend;\nend.\n";
    write_file(&target_path, target);
    write_file(&known_path, known);
    write_file(&maybe_path, maybe);
    write_file(&consumer_path, consumer);

    let mut server = TestServer::launch();
    server.initialize(temp.path(), Value::Null);

    let definition_id = RequestId::from("unknown-conditional-helper-definition".to_string());
    server.send_request(
        definition_id.clone(),
        "textDocument/definition",
        navigation_params(&consumer_path, consumer, "Touch", 0),
    );
    let locations = result_locations(server.response(&definition_id));
    assert!(
        locations.is_empty(),
        "unknown conditional helper must block definition selection: {locations:?}"
    );

    let completion_id = RequestId::from("unknown-conditional-helper-completion".to_string());
    server.send_request(
        completion_id.clone(),
        "textDocument/completion",
        json!({
            "textDocument": {"uri": uri(&consumer_path)},
            "position": position_after(consumer, "Widget.", 0),
        }),
    );
    let completion = server.response(&completion_id);
    let error = completion
        .error
        .expect("unknown conditional helper completion must fail closed");
    assert_eq!(error.code, -32803);
    assert!(
        error
            .message
            .contains("one or more imports could not be resolved")
    );

    server.shutdown();
}

#[test]
fn cyclic_generic_constraints_fail_closed_and_keep_the_server_responsive() {
    let temp = tempfile::tempdir().unwrap();
    let source_path = temp.path().join("CyclicGenericConstraint.pas");
    let source = "unit CyclicGenericConstraint;\ninterface\ntype\n  TNode<T: TNode<T>> = class\n    Value: T;\n  end;\n  TImpl = class(TNode<TImpl>)\n    Member: Integer;\n  end;\nimplementation\nprocedure Run;\nvar\n  Box: TNode<TImpl>;\nbegin\n  Box.Value.Member;\nend;\nend.\n";
    write_file(&source_path, source);

    let mut server = TestServer::launch();
    server.initialize(temp.path(), Value::Null);

    let completion_id = RequestId::from("cyclic-generic-constraint-completion".to_string());
    server.send_request(
        completion_id.clone(),
        "textDocument/completion",
        json!({
            "textDocument": {"uri": uri(&source_path)},
            "position": position_after(source, "Box.Value.", 0),
        }),
    );
    let completion = server.response(&completion_id);
    assert!(
        completion.error.is_none(),
        "cyclic generic constraint completion failed: {completion:?}"
    );
    assert_eq!(
        completion
            .result
            .expect("cyclic generic constraint completion result")["items"],
        json!([])
    );

    let responsive_id = RequestId::from("cyclic-generic-constraint-responsive".to_string());
    server.send_request(
        responsive_id.clone(),
        "textDocument/documentSymbol",
        json!({"textDocument": {"uri": uri(&source_path)}}),
    );
    let responsive = server.response(&responsive_id);
    assert!(
        responsive.error.is_none(),
        "server stopped responding after cyclic generic constraint completion: {responsive:?}"
    );
    server.shutdown();
}

#[test]
fn deeply_parenthesized_overload_request_survives_and_keeps_server_responsive() {
    let temp = tempfile::tempdir().unwrap();
    let source_path = temp.path().join("DeepParenthesizedOverload.pas");
    let mut argument = String::new();
    for _ in 0..65_536 {
        argument.push('(');
    }
    argument.push('1');
    for _ in 0..65_536 {
        argument.push(')');
    }
    let source = format!(
        "unit DeepParenthesizedOverload;\ninterface\ntype\n  TIntResult = class\n    IntMember: Integer;\n  end;\n  TStringResult = class\n    StringMember: Integer;\n  end;\nfunction Pick(Value: Integer): TIntResult; overload;\nfunction Pick(Value: string): TStringResult; overload;\nimplementation\nprocedure Caller;\nbegin\n  Pick({argument}).IntMember;\nend;\nend.\n"
    );
    write_file(&source_path, &source);

    let mut server = TestServer::launch();
    server.initialize(temp.path(), Value::Null);

    let definition_id = RequestId::from("deep-parenthesized-definition".to_string());
    server.send_request(
        definition_id.clone(),
        "textDocument/definition",
        navigation_params(&source_path, &source, "Pick(", 0),
    );
    let definition = result_locations(server.response(&definition_id));
    assert_eq!(
        definition.len(),
        1,
        "deep parenthesized integer call must resolve"
    );

    let responsive_id = RequestId::from("deep-parenthesized-responsive".to_string());
    server.send_request(
        responsive_id.clone(),
        "textDocument/documentSymbol",
        json!({"textDocument": {"uri": uri(&source_path)}}),
    );
    let responsive = server.response(&responsive_id);
    assert!(
        responsive.error.is_none(),
        "server stopped responding after deep parenthesized overload selection: {responsive:?}"
    );
    server.shutdown();
}

#[test]
fn completion_request_marks_unqualified_unknown_ancestry_incomplete() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let source_path = temp.path().join("UnknownUnqualifiedProtocol.pas");
    let source = "unit UnknownUnqualifiedProtocol;\ninterface\ntype\n  TChild = class(TMissing)\n    procedure Run;\n  end;\nimplementation\nprocedure TChild.Run;\nbegin\n  Unknown;\n  Self.Unknown;\nend;\nend.\n";
    write_file(&source_path, source);

    let mut server = TestServer::launch();
    server.initialize(temp.path(), Value::Null);

    let unqualified_id = RequestId::from("unknown-unqualified-completion".to_string());
    server.send_request(
        unqualified_id.clone(),
        "textDocument/completion",
        json!({
            "textDocument": {"uri": uri(&source_path)},
            "position": position_after(source, "  Unknown", 0),
        }),
    );
    let unqualified = server
        .response(&unqualified_id)
        .result
        .expect("unqualified completion result");
    assert_eq!(unqualified["items"], json!([]));
    assert_eq!(unqualified["isIncomplete"], true);

    let qualified_id = RequestId::from("unknown-qualified-completion".to_string());
    server.send_request(
        qualified_id.clone(),
        "textDocument/completion",
        json!({
            "textDocument": {"uri": uri(&source_path)},
            "position": position_after(source, "  Self.Unknown", 0),
        }),
    );
    let qualified = server
        .response(&qualified_id)
        .result
        .expect("qualified completion result");
    assert_eq!(qualified["items"], json!([]));
    assert_eq!(qualified["isIncomplete"], true);

    server.shutdown();
}

#[test]
fn completion_request_rejects_an_unresolved_import_without_partial_items() {
    let temp = tempfile::tempdir().unwrap();
    let source_path = temp.path().join("UnresolvedCompletion.pas");
    let source = "unit UnresolvedCompletion;\ninterface\nuses MissingProvider;\nimplementation\nprocedure Run;\nvar LocalName: Integer;\nbegin\n  Loc;\nend;\nend.\n";
    write_file(&source_path, source);

    let mut server = TestServer::launch();
    server.initialize(temp.path(), Value::Null);
    let id = RequestId::from("unresolved-completion-request".to_string());
    server.send_request(
        id.clone(),
        "textDocument/completion",
        json!({
            "textDocument": {"uri": uri(&source_path)},
            "position": position_after(source, "Loc", 0),
        }),
    );
    let response = server.response(&id);
    let error = response
        .error
        .expect("unresolved import must reject completion");
    assert_eq!(error.code, -32803);
    assert!(
        error
            .message
            .contains("one or more imports could not be resolved")
    );
    assert!(response.result.is_none());
    server.shutdown();
}

#[test]
fn signature_help_request_returns_nested_argument_selection_and_source_labels() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let source_path = temp.path().join("Signature.pas");
    let source = "unit Signature;\ninterface\nprocedure Run(A, B: Integer; C: string; D: Integer); overload;\nprocedure Run(A: string); overload;\nprocedure Other(X, Y: Integer);\nimplementation\nprocedure Run(A, B: Integer; C: string; D: Integer);\nbegin\nend;\nprocedure Run(A: string);\nbegin\nend;\nprocedure Other(X, Y: Integer);\nbegin\nend;\nprocedure Caller;\nbegin\n  Run(Other(1, 2), 'a,b', [1,2], 4);\nend;\nend.\n";
    write_file(&source_path, source);

    let mut server = TestServer::launch();
    server.initialize(temp.path(), Value::Null);
    let id = RequestId::from("signature-help-request".to_string());
    server.send_request(
        id.clone(),
        "textDocument/signatureHelp",
        json!({
            "textDocument": {"uri": uri(&source_path)},
            "position": position_after(source, "[1,2], ", 0),
        }),
    );
    let response = server.response(&id);
    assert!(
        response.error.is_none(),
        "signature help failed: {response:?}"
    );
    let result = response.result.expect("signature help result");
    assert_eq!(result["activeSignature"], Value::Null);
    assert_eq!(result["activeParameter"], 3);
    let signatures = result["signatures"].as_array().expect("signatures");
    assert_eq!(
        signatures
            .iter()
            .map(|signature| signature["label"].as_str().expect("signature label"))
            .collect::<Vec<_>>(),
        [
            "procedure Run(A, B: Integer; C: string; D: Integer);",
            "procedure Run(A: string);",
        ]
    );
    assert_eq!(
        signatures[0]["parameters"]
            .as_array()
            .expect("parameters")
            .iter()
            .map(|parameter| parameter["label"].clone())
            .collect::<Vec<_>>(),
        [
            json!([14, 15]),
            json!([17, 18]),
            json!([29, 30]),
            json!([40, 41])
        ]
    );
    server.shutdown();
}

#[test]
fn generic_signature_help_request_returns_the_generic_source_label() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let source_path = temp.path().join("GenericSignature.pas");
    let source = "unit GenericSignature;\ninterface\nfunction Identity<T>(Value: T): T;\nimplementation\nfunction Identity<T>(Value: T): T;\nbegin\n  Result := Value;\nend;\nprocedure Caller;\nbegin\n  Identity(1);\nend;\nend.\n";
    write_file(&source_path, source);

    let mut server = TestServer::launch();
    server.initialize(temp.path(), Value::Null);
    let id = RequestId::from("generic-signature-help-request".to_string());
    server.send_request(
        id.clone(),
        "textDocument/signatureHelp",
        json!({
            "textDocument": {"uri": uri(&source_path)},
            "position": position_after(source, "Identity(1", 0),
        }),
    );
    let response = server.response(&id);
    assert!(
        response.error.is_none(),
        "generic signature help failed: {response:?}"
    );
    let result = response.result.expect("generic signature help result");
    assert_eq!(result["activeSignature"], 0);
    assert_eq!(result["activeParameter"], 0);
    assert_eq!(
        result["signatures"][0]["label"],
        "function Identity<T>(Value: T): T;"
    );
    server.shutdown();
}

#[test]
fn assistance_requests_follow_provider_overlays_and_newer_versions() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let provider_path = temp.path().join("Provider.pas");
    let main_path = temp.path().join("Main.pas");
    let provider_source = "unit Provider;\ninterface\ntype\n  TWidget = class\n  public\n    DiskMember: Integer;\n  end;\nprocedure DiskRoutine(Value: Integer);\nimplementation\nprocedure DiskRoutine(Value: Integer);\nbegin\nend;\nend.\n";
    let main_source = "unit Main;\ninterface\nuses Provider;\nimplementation\nprocedure Caller;\nvar\n  Obj: TWidget;\nbegin\n  Obj.Disk;\n  DiskRoutine(1);\nend;\nend.\n";
    write_file(&provider_path, provider_source);
    write_file(&main_path, main_source);

    let provider_overlay = provider_source
        .replace("DiskMember", "OverlayMember")
        .replace("DiskRoutine", "OverlayRoutine");
    let main_overlay = main_source
        .replace("Disk", "Overlay")
        .replace("DiskRoutine", "OverlayRoutine");
    let provider_overlay_v2 = provider_overlay
        .replace("OverlayMember", "UpdatedMember")
        .replace("OverlayRoutine", "UpdatedRoutine");
    let main_overlay_v2 = main_overlay
        .replace("Obj.Overlay", "Obj.Updated")
        .replace("OverlayMember", "UpdatedMember")
        .replace("OverlayRoutine", "UpdatedRoutine");

    let mut server = TestServer::launch();
    server.initialize(temp.path(), Value::Null);
    for (path, text) in [
        (&provider_path, provider_overlay.as_str()),
        (&main_path, main_overlay.as_str()),
    ] {
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
    let _ = server.notification("textDocument/publishDiagnostics");
    let _ = server.notification("textDocument/publishDiagnostics");

    let completion_id = RequestId::from("overlay-completion-v1".to_string());
    server.send_request(
        completion_id.clone(),
        "textDocument/completion",
        json!({
            "textDocument": {"uri": uri(&main_path)},
            "position": position_after(&main_overlay, "Obj.Ov", 0)
        }),
    );
    let completion = server.response(&completion_id);
    assert!(
        completion.error.is_none(),
        "overlay completion failed: {completion:?}"
    );
    let completion_result = completion.result.expect("overlay completion result");
    let labels = completion_result["items"]
        .as_array()
        .expect("overlay completion items")
        .iter()
        .map(|item| item["label"].as_str().expect("completion label").to_owned())
        .collect::<Vec<_>>();
    assert_eq!(labels, ["OverlayMember"]);

    let signature_id = RequestId::from("overlay-signature-v1".to_string());
    server.send_request(
        signature_id.clone(),
        "textDocument/signatureHelp",
        json!({
            "textDocument": {"uri": uri(&main_path)},
            "position": position_after(&main_overlay, "OverlayRoutine(", 0)
        }),
    );
    let signature = server.response(&signature_id);
    assert!(
        signature.error.is_none(),
        "overlay signature failed: {signature:?}"
    );
    assert_eq!(
        signature.result.expect("overlay signature result")["signatures"][0]["label"],
        "procedure OverlayRoutine(Value: Integer);"
    );

    for (path, text) in [
        (&provider_path, provider_overlay_v2.as_str()),
        (&main_path, main_overlay_v2.as_str()),
    ] {
        server.send_notification(
            "textDocument/didChange",
            json!({
                "textDocument": {"uri": uri(path), "version": 2},
                "contentChanges": [{"text": text}]
            }),
        );
    }
    let _ = server.notification("textDocument/publishDiagnostics");
    let _ = server.notification("textDocument/publishDiagnostics");

    let completion_id = RequestId::from("overlay-completion-v2".to_string());
    server.send_request(
        completion_id.clone(),
        "textDocument/completion",
        json!({
            "textDocument": {"uri": uri(&main_path)},
            "position": position_after(&main_overlay_v2, "Obj.Up", 0)
        }),
    );
    let completion = server.response(&completion_id);
    assert!(
        completion.error.is_none(),
        "updated overlay completion failed: {completion:?}"
    );
    let completion_result = completion
        .result
        .expect("updated overlay completion result");
    let labels = completion_result["items"]
        .as_array()
        .expect("updated overlay completion items")
        .iter()
        .map(|item| {
            item["label"]
                .as_str()
                .expect("updated completion label")
                .to_owned()
        })
        .collect::<Vec<_>>();
    assert_eq!(labels, ["UpdatedMember"]);

    let signature_id = RequestId::from("overlay-signature-v2".to_string());
    server.send_request(
        signature_id.clone(),
        "textDocument/signatureHelp",
        json!({
            "textDocument": {"uri": uri(&main_path)},
            "position": position_after(&main_overlay_v2, "UpdatedRoutine(", 0)
        }),
    );
    let signature = server.response(&signature_id);
    assert!(
        signature.error.is_none(),
        "updated overlay signature failed: {signature:?}"
    );
    assert_eq!(
        signature.result.expect("updated overlay signature result")["signatures"][0]["label"],
        "procedure UpdatedRoutine(Value: Integer);"
    );

    assert_eq!(
        fs::read(&provider_path).expect("provider disk source"),
        provider_source.as_bytes()
    );
    assert_eq!(
        fs::read(&main_path).expect("main disk source"),
        main_source.as_bytes()
    );
    server.shutdown();
}

#[test]
fn assistance_requests_follow_the_selected_project_provider() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path();
    let main_path = root.join("Main.pas");
    let provider_a_path = root.join("A/Provider.pas");
    let provider_b_path = root.join("B/Provider.pas");
    let project_a_path = root.join("A.dproj");
    let project_b_path = root.join("B.dproj");
    let main_source = "unit Main;\ninterface\nuses Provider;\nimplementation\nprocedure Caller;\nvar\n  Obj: TWidget;\nbegin\n  Obj.;\n  Run(1);\nend;\nend.\n";
    let provider = |member: &str, parameter: &str| {
        format!(
            "unit Provider;\ninterface\ntype\n  TWidget = class\n  public\n    {member}: Integer;\n  end;\nprocedure Run({parameter}: Integer);\nimplementation\nprocedure Run({parameter}: Integer);\nbegin\nend;\nend.\n"
        )
    };
    let provider_a = provider("AMember", "AValue");
    let provider_b = provider("BMember", "BValue");
    let project = |provider_path: &str| {
        format!(
            "<Project><PropertyGroup><MainSource>Main.pas</MainSource></PropertyGroup><ItemGroup><DCCReference Include=\"{provider_path}\" /></ItemGroup></Project>"
        )
    };
    write_file(&main_path, main_source);
    write_file(&provider_a_path, &provider_a);
    write_file(&provider_b_path, &provider_b);
    write_file(&project_a_path, &project("A/Provider.pas"));
    write_file(&project_b_path, &project("B/Provider.pas"));

    let mut server = TestServer::launch();
    server.initialize(root, json!({"projectFile": "A.dproj"}));

    let completion_id = RequestId::from("project-assistance-completion-a".to_string());
    server.send_request(
        completion_id.clone(),
        "textDocument/completion",
        json!({
            "textDocument": {"uri": uri(&main_path)},
            "position": position_after(main_source, "Obj.", 0)
        }),
    );
    let completion = server.response(&completion_id);
    assert!(
        completion.error.is_none(),
        "project A completion failed: {completion:?}"
    );
    let completion_result = completion.result.expect("project A completion result");
    let labels = completion_result["items"]
        .as_array()
        .expect("project A completion items")
        .iter()
        .map(|item| item["label"].as_str().expect("project A completion label"))
        .collect::<Vec<_>>();
    assert_eq!(labels, ["AMember"]);

    let signature_id = RequestId::from("project-assistance-signature-a".to_string());
    server.send_request(
        signature_id.clone(),
        "textDocument/signatureHelp",
        json!({
            "textDocument": {"uri": uri(&main_path)},
            "position": position_after(main_source, "Run(", 0)
        }),
    );
    let signature = server.response(&signature_id);
    assert!(
        signature.error.is_none(),
        "project A signature failed: {signature:?}"
    );
    assert_eq!(
        signature.result.expect("project A signature result")["signatures"][0]["label"],
        "procedure Run(AValue: Integer);"
    );

    let select_id = RequestId::from("project-assistance-select-b".to_string());
    server.send_request(
        select_id.clone(),
        "pascal/selectProject",
        json!({
            "textDocument": {"uri": uri(&main_path)},
            "projectUri": uri(&project_b_path)
        }),
    );
    assert!(server.response(&select_id).error.is_none());

    let completion_id = RequestId::from("project-assistance-completion-b".to_string());
    server.send_request(
        completion_id.clone(),
        "textDocument/completion",
        json!({
            "textDocument": {"uri": uri(&main_path)},
            "position": position_after(main_source, "Obj.", 0)
        }),
    );
    let completion = server.response(&completion_id);
    assert!(
        completion.error.is_none(),
        "project B completion failed: {completion:?}"
    );
    let completion_result = completion.result.expect("project B completion result");
    let labels = completion_result["items"]
        .as_array()
        .expect("project B completion items")
        .iter()
        .map(|item| item["label"].as_str().expect("project B completion label"))
        .collect::<Vec<_>>();
    assert_eq!(labels, ["BMember"]);

    let signature_id = RequestId::from("project-assistance-signature-b".to_string());
    server.send_request(
        signature_id.clone(),
        "textDocument/signatureHelp",
        json!({
            "textDocument": {"uri": uri(&main_path)},
            "position": position_after(main_source, "Run(", 0)
        }),
    );
    let signature = server.response(&signature_id);
    assert!(
        signature.error.is_none(),
        "project B signature failed: {signature:?}"
    );
    assert_eq!(
        signature.result.expect("project B signature result")["signatures"][0]["label"],
        "procedure Run(BValue: Integer);"
    );
    server.shutdown();
}

#[test]
fn hover_request_returns_a_source_excerpt_and_identifier_range() {
    let temp = tempfile::tempdir().unwrap();
    let source_path = temp.path().join("Hover.pas");
    let source = "unit Hover;\ninterface\nprocedure PublicRoutine;\nimplementation\nprocedure PublicRoutine;\nbegin\nend;\nend.\n";
    write_file(&source_path, source);

    let mut server = TestServer::launch();
    server.initialize(temp.path(), Value::Null);
    let id = RequestId::from("hover-request".to_string());
    server.send_request(
        id.clone(),
        "textDocument/hover",
        navigation_params(&source_path, source, "PublicRoutine", 1),
    );
    let response = server.response(&id);
    assert!(response.error.is_none(), "hover failed: {response:?}");
    let result = response.result.expect("hover result");
    assert_eq!(
        result["range"],
        json!({
            "start": {"line": 4, "character": 10},
            "end": {"line": 4, "character": 23}
        })
    );
    assert_eq!(result["contents"]["kind"], "plaintext");
    assert!(
        result["contents"]["value"]
            .as_str()
            .expect("hover text")
            .contains("procedure PublicRoutine;")
    );
    server.shutdown();
}

#[test]
fn hover_negotiates_markdown_when_the_client_advertises_it() {
    let temp = tempfile::tempdir().unwrap();
    let source_path = temp.path().join("MarkdownHover.pas");
    let source = "unit MarkdownHover;\ninterface\nprocedure PublicRoutine;\nimplementation\nprocedure PublicRoutine;\nbegin\nend;\nend.\n";
    write_file(&source_path, source);

    let mut server = TestServer::launch();
    let initialize_id = RequestId::from("markdown-initialize".to_string());
    server.send_request(
        initialize_id.clone(),
        "initialize",
        json!({
            "processId": null,
            "rootUri": uri(temp.path()),
            "capabilities": {
                "general": {"positionEncodings": ["utf-16"]},
                "textDocument": {
                    "hover": {"contentFormat": ["markdown", "plaintext"]}
                }
            }
        }),
    );
    let initialize = server.response(&initialize_id);
    assert!(
        initialize.error.is_none(),
        "initialize failed: {initialize:?}"
    );
    server.send_notification("initialized", json!({}));

    let id = RequestId::from("markdown-hover-request".to_string());
    server.send_request(
        id.clone(),
        "textDocument/hover",
        navigation_params(&source_path, source, "PublicRoutine", 0),
    );
    let response = server.response(&id);
    assert!(response.error.is_none(), "hover failed: {response:?}");
    let contents = &response.result.expect("hover result")["contents"];
    assert_eq!(contents["kind"], "markdown");
    assert!(contents["value"].as_str().unwrap().contains("```pascal"));
    server.shutdown();
}

#[test]
fn documentation_formats_are_negotiated_independently_for_all_assistance_endpoints() {
    let temp = tempfile::tempdir().unwrap();
    let source_path = temp.path().join("DocumentationFormats.pas");
    let source = "unit DocumentationFormats;\ninterface\n/// <summary>Returns <c>the value</c>.</summary>\n/// <param name=\"Name\">The lookup name.</param>\nfunction ValueFor(Name: string): Integer;\nprocedure Caller;\nimplementation\nfunction ValueFor(Name: string): Integer;\nbegin\n  Result := 1;\nend;\nprocedure Caller;\nvar\n  /// <summary>Local value.</summary>\n  LocalValue: Integer;\nbegin\n  Loc\n  ValueFor('text' );\nend;\nend.\n";
    write_file(&source_path, source);

    let mut server = TestServer::launch();
    let initialize_id = RequestId::from("documentation-formats-initialize".to_string());
    server.send_request(
        initialize_id.clone(),
        "initialize",
        json!({
            "processId": null,
            "rootUri": uri(temp.path()),
            "capabilities": {
                "general": {"positionEncodings": ["utf-16"]},
                "textDocument": {
                    "hover": {"contentFormat": ["plaintext"]},
                    "completion": {
                        "completionItem": {"documentationFormat": ["markdown"]}
                    },
                    "signatureHelp": {
                        "signatureInformation": {
                            "documentationFormat": ["markdown", "plaintext"]
                        }
                    }
                }
            }
        }),
    );
    let initialize = server.response(&initialize_id);
    assert!(
        initialize.error.is_none(),
        "initialize failed: {initialize:?}"
    );
    server.send_notification("initialized", json!({}));

    let hover_id = RequestId::from("documentation-formats-hover".to_string());
    server.send_request(
        hover_id.clone(),
        "textDocument/hover",
        navigation_params(&source_path, source, "ValueFor", 0),
    );
    let hover = server.response(&hover_id);
    assert!(hover.error.is_none(), "hover failed: {hover:?}");
    let hover_contents = &hover.result.expect("hover result")["contents"];
    assert_eq!(hover_contents["kind"], "plaintext");
    assert!(
        hover_contents["value"]
            .as_str()
            .expect("hover plaintext")
            .contains("Returns the value.")
    );

    let completion_id = RequestId::from("documentation-formats-completion".to_string());
    server.send_request(
        completion_id.clone(),
        "textDocument/completion",
        json!({
            "textDocument": {"uri": uri(&source_path)},
            "position": position_after(source, "  Loc", 0),
        }),
    );
    let completion = server.response(&completion_id);
    assert!(
        completion.error.is_none(),
        "completion failed: {completion:?}"
    );
    let completion_result = completion.result.expect("completion result");
    let completion_item = completion_result["items"]
        .as_array()
        .expect("completion items")
        .iter()
        .find(|item| item["label"] == "LocalValue")
        .cloned()
        .unwrap_or_else(|| panic!("documented completion item missing: {completion_result}"));
    assert_eq!(completion_item["documentation"]["kind"], "markdown");
    assert_eq!(completion_item["documentation"]["value"], "Local value.");

    let signature_id = RequestId::from("documentation-formats-signature".to_string());
    server.send_request(
        signature_id.clone(),
        "textDocument/signatureHelp",
        json!({
            "textDocument": {"uri": uri(&source_path)},
            "position": position_after(source, "ValueFor('text' ", 0),
        }),
    );
    let signature = server.response(&signature_id);
    assert!(signature.error.is_none(), "signature failed: {signature:?}");
    let signature = signature.result.expect("signature result");
    assert_eq!(
        signature["signatures"][0]["documentation"],
        json!({"kind": "markdown", "value": "Returns `the value`."})
    );
    assert_eq!(
        signature["signatures"][0]["parameters"][0]["documentation"],
        json!({"kind": "markdown", "value": "The lookup name."})
    );
    server.shutdown();
}

#[test]
fn bounded_documentation_expansion_keeps_the_server_responsive() {
    let temp = tempfile::tempdir().unwrap();
    let source_path = temp.path().join("BoundedDocumentation.pas");
    let names = std::iter::repeat_n("X", 1500).collect::<Vec<_>>().join(",");
    let value = "x".repeat(60_000);
    let source = format!(
        "unit BoundedDocumentation;\ninterface\n/// <param name=\"{names}\">{value}</param>\nprocedure Safe;\nimplementation\nprocedure Safe;\nbegin\nend;\nend.\n"
    );
    write_file(&source_path, &source);

    let mut server = TestServer::launch();
    server.initialize(temp.path(), Value::Null);

    for request_id in [
        RequestId::from("bounded-documentation-first".to_string()),
        RequestId::from("bounded-documentation-second".to_string()),
    ] {
        server.send_request(
            request_id.clone(),
            "textDocument/hover",
            navigation_params(&source_path, &source, "Safe", 0),
        );
        let response = server.response(&request_id);
        assert!(
            response.error.is_none(),
            "bounded hover failed: {response:?}"
        );
        assert!(
            response.result.is_some(),
            "bounded hover returned no result"
        );
    }
    server.shutdown();
}

#[test]
fn hover_markdown_escapes_backtick_fences_inside_multiline_comments() {
    let temp = tempfile::tempdir().unwrap();
    let source_path = temp.path().join("FenceHover.pas");
    let source = "unit FenceHover;\ninterface\nprocedure Run(\n  { Example:\n  ```\n  **This is Pascal comment text, not Markdown.**\n  ```\n  }\n  Arg: Integer);\nimplementation\nend.\n";
    write_file(&source_path, source);

    let mut server = TestServer::launch();
    let initialize_id = RequestId::from("fence-initialize".to_string());
    server.send_request(
        initialize_id.clone(),
        "initialize",
        json!({
            "processId": null,
            "rootUri": uri(temp.path()),
            "capabilities": {
                "general": {"positionEncodings": ["utf-16"]},
                "textDocument": {
                    "hover": {"contentFormat": ["markdown", "plaintext"]}
                }
            }
        }),
    );
    let initialize = server.response(&initialize_id);
    assert!(
        initialize.error.is_none(),
        "initialize failed: {initialize:?}"
    );
    server.send_notification("initialized", json!({}));

    let id = RequestId::from("fence-hover-request".to_string());
    server.send_request(
        id.clone(),
        "textDocument/hover",
        navigation_params(&source_path, source, "Run", 0),
    );
    let response = server.response(&id);
    assert!(response.error.is_none(), "hover failed: {response:?}");
    let result = response.result.expect("hover result");
    let value = result["contents"]["value"]
        .as_str()
        .expect("markdown hover value");
    let opening = value
        .lines()
        .find(|line| line.ends_with("pascal"))
        .expect("Pascal code fence");
    let fence = opening.strip_suffix("pascal").expect("fence prefix");
    assert!(fence.len() > 3, "collision-safe fence required: {value}");
    assert_eq!(value.lines().last(), Some(fence));
    assert!(
        value.contains("**This is Pascal comment text, not Markdown.**"),
        "{value}"
    );
    server.shutdown();
}

#[test]
fn hover_rejects_invalid_params_and_returns_null_for_unsupported_positions() {
    let temp = tempfile::tempdir().unwrap();
    let source_path = temp.path().join("UnsupportedHover.pas");
    let source = "unit UnsupportedHover;\ninterface\nconst Value = 1;\nimplementation\nprocedure Run;\nbegin\n  // Value\n  Unknown.Value;\nend;\nend.\n";
    write_file(&source_path, source);

    let mut server = TestServer::launch();
    server.initialize(temp.path(), Value::Null);

    let invalid_id = RequestId::from("invalid-hover".to_string());
    server.send_request(invalid_id.clone(), "textDocument/hover", json!({}));
    let invalid = server.response(&invalid_id);
    assert_eq!(invalid.error.expect("invalid params error").code, -32602);

    for (id_text, position) in [
        ("comment-hover", position_of(source, "Value", 1)),
        ("unknown-receiver-hover", position_of(source, "Value", 2)),
    ] {
        let id = RequestId::from(id_text.to_string());
        server.send_request(
            id.clone(),
            "textDocument/hover",
            json!({"textDocument": {"uri": uri(&source_path)}, "position": position}),
        );
        let response = server.response(&id);
        assert!(
            response.error.is_none(),
            "unsupported hover failed: {response:?}"
        );
        assert!(response.result.as_ref().is_none_or(Value::is_null));
    }
    server.shutdown();
}

#[test]
fn hover_uses_an_unsaved_provider_overlay_for_imported_declarations() {
    let temp = tempfile::tempdir().unwrap();
    let provider = temp.path().join("Provider.pas");
    let consumer = temp.path().join("Consumer.pas");
    let disk_provider = "unit Provider;\ninterface\ntype\n  TWidget = class\n    property DiskValue: Integer;\n  end;\nimplementation\nend.\n";
    let overlay_provider = "unit Provider;\ninterface\ntype\n  TWidget = class\n    property OverlayValue: Integer;\n  end;\nimplementation\nend.\n";
    let consumer_source = "unit Consumer;\ninterface\nuses Provider;\nimplementation\nprocedure Run;\nvar Widget: Provider.TWidget;\nbegin\n  Log(Widget.OverlayValue);\nend;\nend.\n";
    write_file(&provider, disk_provider);
    write_file(&consumer, consumer_source);

    let mut server = TestServer::launch();
    server.initialize(temp.path(), Value::Null);
    server.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri(&provider),
                "languageId": "pascal",
                "version": 3,
                "text": overlay_provider
            }
        }),
    );

    let id = RequestId::from("overlay-hover".to_string());
    server.send_request(
        id.clone(),
        "textDocument/hover",
        navigation_params(&consumer, consumer_source, "OverlayValue", 0),
    );
    let response = server.response(&id);
    assert!(response.error.is_none(), "hover failed: {response:?}");
    let result = response.result.expect("hover result");
    let value = result["contents"]["value"]
        .as_str()
        .expect("plaintext hover value");
    assert!(value.contains("OverlayValue: Integer"), "{value}");
    assert!(!value.contains("DiskValue"), "{value}");
    server.shutdown();
}

#[test]
fn provider_overlay_documentation_refreshes_after_did_change() {
    let temp = tempfile::tempdir().unwrap();
    let provider = temp.path().join("Provider.pas");
    let consumer = temp.path().join("Consumer.pas");
    let disk_provider = "unit Provider;\ninterface\ntype\n  TWidget = class\n    property OverlayValue: Integer;\n  end;\nimplementation\nend.\n";
    let overlay_v1 = "unit Provider;\ninterface\ntype\n  TWidget = class\n    /// <summary>First overlay documentation.</summary>\n    property OverlayValue: Integer;\n  end;\nimplementation\nend.\n";
    let overlay_v2 = "unit Provider;\ninterface\ntype\n  TWidget = class\n    /// <summary>Updated overlay documentation.</summary>\n    property OverlayValue: Integer;\n  end;\nimplementation\nend.\n";
    let consumer_source = "unit Consumer;\ninterface\nuses Provider;\nimplementation\nprocedure Run;\nvar Widget: Provider.TWidget;\nbegin\n  Log(Widget.OverlayValue);\nend;\nend.\n";
    write_file(&provider, disk_provider);
    write_file(&consumer, consumer_source);

    let mut server = TestServer::launch();
    server.initialize(temp.path(), Value::Null);
    server.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri(&provider),
                "languageId": "pascal",
                "version": 3,
                "text": overlay_v1
            }
        }),
    );

    let first_id = RequestId::from("overlay-documentation-first".to_string());
    server.send_request(
        first_id.clone(),
        "textDocument/hover",
        navigation_params(&consumer, consumer_source, "OverlayValue", 0),
    );
    let first = server.response(&first_id);
    assert!(first.error.is_none(), "first hover failed: {first:?}");
    let first_result = first.result.expect("first hover");
    let first_value = first_result["contents"]["value"]
        .as_str()
        .expect("first hover text")
        .to_owned();
    assert!(
        first_value.contains("First overlay documentation."),
        "{first_value}"
    );

    server.send_notification(
        "textDocument/didChange",
        json!({
            "textDocument": {"uri": uri(&provider), "version": 4},
            "contentChanges": [{"text": overlay_v2}]
        }),
    );
    let second_id = RequestId::from("overlay-documentation-second".to_string());
    server.send_request(
        second_id.clone(),
        "textDocument/hover",
        navigation_params(&consumer, consumer_source, "OverlayValue", 0),
    );
    let second = server.response(&second_id);
    assert!(second.error.is_none(), "second hover failed: {second:?}");
    let second_result = second.result.expect("second hover");
    let second_value = second_result["contents"]["value"]
        .as_str()
        .expect("second hover text")
        .to_owned();
    assert!(
        second_value.contains("Updated overlay documentation."),
        "{second_value}"
    );
    assert!(
        !second_value.contains("First overlay documentation."),
        "{second_value}"
    );
    server.shutdown();
}

#[test]
fn type_definition_request_returns_the_source_type_declaration() {
    let temp = tempfile::tempdir().unwrap();
    let provider = temp.path().join("Provider.pas");
    let consumer = temp.path().join("Consumer.pas");
    let provider_source =
        "unit Provider;\ninterface\ntype\n  TFoo = class\n  end;\nimplementation\nend.\n";
    let consumer_source = "unit Consumer;\ninterface\nuses Provider;\nimplementation\nprocedure Run;\nvar Item: TFoo;\nbegin\n  Item := nil;\nend;\nend.\n";
    write_file(&provider, provider_source);
    write_file(&consumer, consumer_source);

    let mut server = TestServer::launch();
    server.initialize(temp.path(), Value::Null);
    let id = RequestId::from("type-definition-request".to_string());
    server.send_request(
        id.clone(),
        "textDocument/typeDefinition",
        navigation_params(&consumer, consumer_source, "Item :=", 0),
    );
    let response = server.response(&id);
    assert!(
        response.error.is_none(),
        "type definition failed: {response:?}"
    );
    let locations = response
        .result
        .expect("type definition result")
        .as_array()
        .expect("type definition location array")
        .clone();
    assert_eq!(
        locations,
        vec![json!({
            "uri": uri(&provider),
            "range": {
                "start": {"line": 3, "character": 2},
                "end": {"line": 3, "character": 6}
            }
        })]
    );
    server.shutdown();
}

#[test]
fn type_definition_request_resolves_a_function_result_with_utf16_positions() {
    let temp = tempfile::tempdir().unwrap();
    let provider = temp.path().join("ResultProvider.pas");
    let consumer = temp.path().join("ResultConsumer.pas");
    let provider_source = "unit ResultProvider;\ninterface\ntype\n  TResult = class\n    Member: Integer;\n  end;\nfunction MakeValue: TResult;\nimplementation\nfunction MakeValue: TResult;\nbegin\n  Result := TResult.Create;\nend;\nend.\n";
    let consumer_source = "unit ResultConsumer;\ninterface\nuses ResultProvider;\nimplementation\nprocedure Run;\nbegin\n  {😀} ResultProvider.MakeValue().Member := 1;\nend;\nend.\n";
    write_file(&provider, provider_source);
    write_file(&consumer, consumer_source);

    let mut server = TestServer::launch();
    server.initialize(temp.path(), Value::Null);
    let id = RequestId::from("function-result-type-definition".to_string());
    server.send_request(
        id.clone(),
        "textDocument/typeDefinition",
        navigation_params(&consumer, consumer_source, "MakeValue", 0),
    );
    let response = server.response(&id);
    assert!(
        response.error.is_none(),
        "function result type definition failed: {response:?}"
    );
    let result = response
        .result
        .expect("function result type definition result");
    let locations = result
        .as_array()
        .expect("function result type definition locations");
    assert_eq!(
        locations,
        &vec![json!({
            "uri": uri(&provider),
            "range": {
                "start": {"line": 3, "character": 2},
                "end": {"line": 3, "character": 9}
            }
        })]
    );
    server.shutdown();
}

#[test]
fn type_definition_returns_empty_for_primitives_unknowns_and_malformed_positions() {
    let temp = tempfile::tempdir().unwrap();
    let source_path = temp.path().join("UnsupportedTypeDefinition.pas");
    let source = "unit UnsupportedTypeDefinition;\ninterface\nimplementation\nprocedure Run;\nvar Item: Integer; UnknownItem: MissingType;\nbegin\n  Item := 1;\n  UnknownItem := Item;\nend;\nend.\n";
    write_file(&source_path, source);

    let mut server = TestServer::launch();
    server.initialize(temp.path(), Value::Null);

    let invalid_id = RequestId::from("invalid-type-definition".to_string());
    server.send_request(invalid_id.clone(), "textDocument/typeDefinition", json!({}));
    let invalid = server.response(&invalid_id);
    assert_eq!(invalid.error.expect("invalid params error").code, -32602);

    for (id_text, position) in [
        (
            "primitive-type-definition",
            position_of(source, "Item :=", 0),
        ),
        (
            "unknown-type-definition",
            position_of(source, "UnknownItem :=", 0),
        ),
        ("outside-type-definition", Position::new(100, 0)),
    ] {
        let id = RequestId::from(id_text.to_string());
        server.send_request(
            id.clone(),
            "textDocument/typeDefinition",
            json!({
                "textDocument": {"uri": uri(&source_path)},
                "position": position
            }),
        );
        let response = server.response(&id);
        assert!(
            response.error.is_none(),
            "unsupported type definition failed: {response:?}"
        );
        assert!(response.result.as_ref().is_none_or(|value| {
            value.is_null() || value.as_array().is_some_and(Vec::is_empty)
        }));
    }
    server.shutdown();
}

#[test]
fn type_definition_uses_an_unsaved_provider_overlay() {
    let temp = tempfile::tempdir().unwrap();
    let provider = temp.path().join("Provider.pas");
    let consumer = temp.path().join("Consumer.pas");
    let disk_provider =
        "unit Provider;\ninterface\ntype\n  TDisk = class end;\nimplementation\nend.\n";
    let overlay_provider =
        "unit Provider;\ninterface\ntype\n  TOverlay = class end;\nimplementation\nend.\n";
    let consumer_source = "unit Consumer;\ninterface\nuses Provider;\nimplementation\nprocedure Run;\nvar Item: TOverlay;\nbegin\n  Item := nil;\nend;\nend.\n";
    write_file(&provider, disk_provider);
    write_file(&consumer, consumer_source);

    let mut server = TestServer::launch();
    server.initialize(temp.path(), Value::Null);
    server.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri(&provider),
                "languageId": "pascal",
                "version": 3,
                "text": overlay_provider
            }
        }),
    );

    let id = RequestId::from("overlay-type-definition".to_string());
    server.send_request(
        id.clone(),
        "textDocument/typeDefinition",
        navigation_params(&consumer, consumer_source, "Item :=", 0),
    );
    let response = server.response(&id);
    assert!(
        response.error.is_none(),
        "overlay type definition failed: {response:?}"
    );
    assert_eq!(
        response.result.as_ref().expect("overlay result")[0]["range"]["start"]["line"],
        3
    );
    assert_eq!(
        response.result.as_ref().expect("overlay result")[0]["uri"],
        uri(&provider).to_string()
    );

    server.shutdown();
}

#[test]
fn type_definition_rejects_anonymous_types_and_unrelated_qualified_locals() {
    let temp = tempfile::tempdir().unwrap();
    let source_path = temp.path().join("TypeDefinitionReview.pas");
    let source = r#"unit TypeDefinitionReview;
interface
type
  TFoo = class end;
  TArrayAlias = array of TFoo;
  TPointerAlias = ^TFoo;
implementation
procedure Hidden;
type
  TFoo = record end;
begin
end;
procedure Run;
var
  Direct: TFoo;
  Many: array of TFoo;
  Ptr: ^TFoo;
  NamedMany: TArrayAlias;
  NamedPtr: TPointerAlias;
  Qualified: TypeDefinitionReview.TFoo;
begin
  Direct := nil;
  Many := nil;
  Ptr := nil;
  NamedMany := nil;
  NamedPtr := nil;
  Qualified := nil;
end;
end.
"#;
    write_file(&source_path, source);

    let mut server = TestServer::launch();
    server.initialize(temp.path(), Value::Null);

    let mut locations_at = |id_text: &str, position: Position| {
        let id = RequestId::from(id_text.to_owned());
        server.send_request(
            id.clone(),
            "textDocument/typeDefinition",
            json!({
                "textDocument": {"uri": uri(&source_path)},
                "position": position,
            }),
        );
        result_locations(server.response(&id))
    };

    let direct = locations_at("review-direct", position_of(source, "Direct: TFoo", 0));
    assert_exact_location_signatures(
        &direct,
        vec![expected_location_signature(&source_path, source, "TFoo", 0)],
    );

    for (id, declaration) in [
        ("review-anonymous-array", "Many: array of TFoo"),
        ("review-anonymous-pointer", "Ptr: ^TFoo"),
    ] {
        assert!(
            locations_at(id, position_of(source, declaration, 0)).is_empty(),
            "anonymous type {declaration:?} must not return a contained type target"
        );
    }

    let named_array = locations_at(
        "review-named-array-alias",
        position_of(source, "NamedMany: TArrayAlias", 0),
    );
    assert_exact_location_signatures(
        &named_array,
        vec![expected_location_signature(
            &source_path,
            source,
            "TArrayAlias",
            0,
        )],
    );

    let named_pointer = locations_at(
        "review-named-pointer-alias",
        position_of(source, "NamedPtr: TPointerAlias", 0),
    );
    assert_exact_location_signatures(
        &named_pointer,
        vec![expected_location_signature(
            &source_path,
            source,
            "TPointerAlias",
            0,
        )],
    );

    let qualified_variable = locations_at(
        "review-qualified-variable",
        position_of(source, "Qualified: TypeDefinitionReview.TFoo", 0),
    );
    assert_exact_location_signatures(
        &qualified_variable,
        vec![expected_location_signature(&source_path, source, "TFoo", 0)],
    );

    let qualified_type = locations_at(
        "review-qualified-type",
        final_qualified_type_position(source, "TypeDefinitionReview.TFoo"),
    );
    assert_exact_location_signatures(
        &qualified_type,
        vec![expected_location_signature(&source_path, source, "TFoo", 0)],
    );

    server.shutdown();
}

#[test]
fn type_definition_cancellation_handles_a_large_source_request() {
    let temp = tempfile::tempdir().unwrap();
    let source_path = temp.path().join("ManyTypeDefinitionUses.pas");
    let mut source = String::from(
        "unit ManyTypeDefinitionUses;\ninterface\ntype TItem = record end;\nimplementation\nprocedure Run;\nvar Item: TItem;\nbegin\n",
    );
    for _ in 0..20_000 {
        source.push_str("  Item := Item;\n");
    }
    source.push_str("end;\nend.\n");
    write_file(&source_path, &source);

    let mut server = TestServer::launch();
    server.initialize(temp.path(), Value::Null);
    let id = RequestId::from("cancelled-type-definition".to_string());
    server.send_request(
        id.clone(),
        "textDocument/typeDefinition",
        navigation_params(&source_path, &source, "Item :=", 0),
    );
    server.send_notification(
        "$/cancelRequest",
        json!({"id": "cancelled-type-definition"}),
    );
    let response = server.response(&id);
    let error = response
        .error
        .expect("cancelled type definition query must fail");
    assert_eq!(error.code, -32800);
    assert_eq!(error.message, "request cancelled");
    server.shutdown();
}

#[test]
fn document_symbols_describe_class_and_method_ranges() {
    let temp = tempfile::tempdir().unwrap();
    let main = temp.path().join("Main.pas");
    let source = "unit Main;\ninterface\ntype TWidget = class\nprocedure Run;\nend;\nimplementation\nprocedure TWidget.Run;\nbegin\nend;\nend.\n";
    write_file(&main, source);
    let mut server = TestServer::launch();
    server.initialize(temp.path(), Value::Null);
    let id = RequestId::from("outline".to_string());
    server.send_request(
        id.clone(),
        "textDocument/documentSymbol",
        json!({"textDocument": {"uri": uri(&main)}}),
    );
    let response = server.response(&id);
    assert!(response.error.is_none(), "{:?}", response.error);
    let symbols = response.result.unwrap().as_array().unwrap().clone();
    assert_eq!(
        symbols
            .iter()
            .map(|symbol| symbol["name"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["Main", "TWidget", "Run", "Run"]
    );
    assert_eq!(
        symbols
            .iter()
            .map(|symbol| symbol["kind"].as_u64().unwrap())
            .collect::<Vec<_>>(),
        [2, 5, 6, 6]
    );
    assert_eq!(
        symbols[1]["location"]["range"],
        json!({"start": {"line": 2, "character": 5}, "end": {"line": 4, "character": 4}})
    );
    assert_eq!(
        symbols[2]["location"]["range"],
        json!({"start": {"line": 3, "character": 0}, "end": {"line": 3, "character": 14}})
    );
    assert_eq!(
        symbols[3]["location"]["range"],
        json!({"start": {"line": 6, "character": 0}, "end": {"line": 8, "character": 4}})
    );
    server.shutdown();
}

#[test]
fn document_symbols_are_hierarchical_when_the_client_supports_it() {
    let temp = tempfile::tempdir().unwrap();
    let main = temp.path().join("Main.pas");
    let source = "unit Main;\ninterface\ntype TWidget = class\nprocedure Run;\nend;\nimplementation\nprocedure TWidget.Run;\nbegin\nend;\nend.\n";
    write_file(&main, source);
    let mut server = TestServer::launch();
    server.initialize_with_hierarchical_document_symbols(temp.path());

    let id = RequestId::from("hierarchical-outline".to_string());
    server.send_request(
        id.clone(),
        "textDocument/documentSymbol",
        json!({"textDocument": {"uri": uri(&main)}}),
    );
    let response = server.response(&id);
    assert!(response.error.is_none(), "{:?}", response.error);
    let symbols = response.result.unwrap().as_array().unwrap().clone();
    assert_eq!(symbols.len(), 1);
    assert_eq!(symbols[0]["name"], "Main");
    assert_eq!(
        symbols[0]["selectionRange"],
        json!({"start": {"line": 0, "character": 5}, "end": {"line": 0, "character": 9}})
    );
    assert_eq!(
        symbols[0]["children"]
            .as_array()
            .unwrap()
            .iter()
            .map(|symbol| symbol["name"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["TWidget", "Run"]
    );
    let widget = &symbols[0]["children"][0];
    assert_eq!(
        widget["selectionRange"],
        json!({"start": {"line": 2, "character": 5}, "end": {"line": 2, "character": 12}})
    );
    assert_eq!(widget["children"][0]["name"], "Run");
    assert_eq!(
        widget["children"][0]["selectionRange"],
        json!({"start": {"line": 3, "character": 10}, "end": {"line": 3, "character": 13}})
    );
    assert_eq!(symbols[0]["children"][1]["children"], Value::Null);
    server.shutdown();
}

#[test]
fn document_symbol_requests_reject_excessive_hierarchy_depth() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("workspace");
    let source_path = root.join("DeepSymbols.pas");
    let mut source = String::from("unit DeepSymbols;\ninterface\nimplementation\nprocedure P0;\n");
    for index in 1..40 {
        source.push_str(&format!("{}procedure P{index};\n", "  ".repeat(index)));
    }
    for index in (1..40).rev() {
        let indent = "  ".repeat(index);
        source.push_str(&format!("{indent}begin\n{indent}end;\n"));
    }
    source.push_str("begin\nend;\nend.\n");
    write_file(&source_path, &source);

    let mut server = TestServer::launch();
    server.initialize(&root, Value::Null);
    let id = RequestId::from("deep-document-symbols".to_string());
    server.send_request(
        id.clone(),
        "textDocument/documentSymbol",
        json!({"textDocument": {"uri": uri(&source_path)}}),
    );
    let response = server.response(&id);
    let error = response
        .error
        .expect("excessive document symbol depth must fail closed");
    assert!(error.message.contains("hierarchy"));
    assert!(error.message.contains("32"));
    server.shutdown();
}

#[test]
fn workspace_symbols_search_unopened_sources_and_skip_routine_locals() {
    let temp = tempfile::tempdir().unwrap();
    let main = temp.path().join("Main.pas");
    let other = temp.path().join("Other.pas");
    write_file(
        &main,
        "unit Main;\ninterface\nprocedure GlobalThing;\nimplementation\nprocedure GlobalThing;\nvar LocalThing: Integer;\nbegin\n  LocalThing := 1;\nend;\nend.\n",
    );
    write_file(
        &other,
        "unit Other;\ninterface\ntype TWidget = class\n  Value: Integer;\nend;\nimplementation\nend.\n",
    );
    let mut server = TestServer::launch();
    server.initialize(temp.path(), Value::Null);

    let id = RequestId::from("workspace-symbols".to_string());
    server.send_request(id.clone(), "workspace/symbol", json!({"query": "WIDGET"}));
    let response = server.response(&id);
    assert!(response.error.is_none(), "{:?}", response.error);
    let symbols = response.result.unwrap().as_array().unwrap().clone();
    assert_eq!(symbols.len(), 1);
    assert_eq!(symbols[0]["name"], "TWidget");
    assert_eq!(symbols[0]["containerName"], "Other");
    assert_eq!(symbols[0]["location"]["uri"], uri(&other).to_string());

    let id = RequestId::from("workspace-symbols-local".to_string());
    server.send_request(
        id.clone(),
        "workspace/symbol",
        json!({"query": "LocalThing"}),
    );
    let response = server.response(&id);
    assert!(response.error.is_none(), "{:?}", response.error);
    assert!(response.result.unwrap().as_array().unwrap().is_empty());
    server.shutdown();
}

#[test]
fn references_include_unopened_consumers() {
    let temp = tempfile::tempdir().unwrap();
    let provider = temp.path().join("Provider.pas");
    let consumer = temp.path().join("Consumer.pas");
    let unrelated = temp.path().join("Unrelated.pas");
    let provider_source =
        "unit Provider;\ninterface\nconst SharedValue = 1;\nimplementation\nend.\n";
    let consumer_source = "unit Consumer;\ninterface\nuses Provider;\nimplementation\nprocedure Run;\nbegin\n  Log(SharedValue);\n  Log(Provider.SharedValue);\nend;\nend.\n";
    let unrelated_source =
        "unit Unrelated;\ninterface\nconst SharedValue = 2;\nimplementation\nend.\n";
    write_file(&provider, provider_source);
    write_file(&consumer, consumer_source);
    write_file(&unrelated, unrelated_source);

    let mut server = TestServer::launch();
    server.initialize(temp.path(), Value::Null);
    let id = RequestId::from("bound-references".to_string());
    server.send_request(
        id.clone(),
        "textDocument/references",
        json!({
            "textDocument": {"uri": uri(&provider)},
            "position": position_of(provider_source, "SharedValue", 0),
            "context": {"includeDeclaration": false}
        }),
    );
    let response = server.response(&id);
    assert!(response.error.is_none(), "{:?}", response.error);
    let locations = response.result.unwrap();
    assert_eq!(locations.as_array().unwrap().len(), 2);
    assert!(
        locations
            .as_array()
            .unwrap()
            .iter()
            .all(|location| location["uri"] == uri(&consumer).to_string())
    );

    let id = RequestId::from("bound-references-with-declaration".to_string());
    server.send_request(
        id.clone(),
        "textDocument/references",
        json!({
            "textDocument": {"uri": uri(&provider)},
            "position": position_of(provider_source, "SharedValue", 0),
            "context": {"includeDeclaration": true}
        }),
    );
    let response = server.response(&id);
    assert!(response.error.is_none(), "{response:?}");
    let locations = response.result.unwrap();
    assert_eq!(locations.as_array().unwrap().len(), 3);
    assert_eq!(locations[0]["uri"], uri(&consumer).to_string());
    assert_eq!(locations[1]["uri"], uri(&consumer).to_string());
    assert_eq!(locations[2]["uri"], uri(&provider).to_string());
    server.shutdown();
}

#[test]
fn references_reject_a_variable_rhs_in_a_cast_receiver() {
    let temp = tempfile::tempdir().unwrap();
    let source_path = temp.path().join("InvalidCastReferences.pas");
    let source = "unit InvalidCastReferences;\ninterface\ntype\n  TWidget = class\n    Member: Integer;\n  end;\n  TOther = class\n    Member: Integer;\n  end;\nimplementation\nprocedure Caller;\nvar\n  Obj: TWidget;\n  OtherObj: TOther;\nbegin\n  (Obj as OtherObj).Member := 1;\nend;\nend.\n";
    write_file(&source_path, source);

    let mut server = TestServer::launch();
    server.initialize(temp.path(), Value::Null);
    let id = RequestId::from("invalid-cast-references".to_string());
    server.send_request(
        id.clone(),
        "textDocument/references",
        json!({
            "textDocument": {"uri": uri(&source_path)},
            "position": position_of(source, "Member", 1),
            "context": {"includeDeclaration": false}
        }),
    );
    let response = server.response(&id);
    assert!(
        response.error.is_some(),
        "an unresolved cast receiver must not return references"
    );
    server.shutdown();
}

#[test]
fn references_reject_a_shadowed_qualified_cast_type_root() {
    let temp = tempfile::tempdir().unwrap();
    let provider_path = temp.path().join("CastTypes.pas");
    let consumer_path = temp.path().join("QualifiedCastRootShadow.pas");
    let provider =
        "unit CastTypes;\ninterface\ntype\n  TResult = class\n    Member: Integer;\n  end;\nend.\n";
    let consumer = "unit QualifiedCastRootShadow;\ninterface\nuses CastTypes;\ntype\n  TWidget = class\n  end;\nprocedure Caller;\nvar\n  Obj: TWidget;\n  CastTypes: Integer;\nbegin\n  (Obj as CastTypes.TResult).Member := 1;\nend;\nend.\n";
    write_file(&provider_path, provider);
    write_file(&consumer_path, consumer);

    let mut server = TestServer::launch();
    server.initialize(temp.path(), Value::Null);
    let id = RequestId::from("shadowed-qualified-cast-references".to_string());
    server.send_request(
        id.clone(),
        "textDocument/references",
        json!({
            "textDocument": {"uri": uri(&consumer_path)},
            "position": position_of(consumer, "Member", 0),
            "context": {"includeDeclaration": false}
        }),
    );
    let response = server.response(&id);
    assert!(
        response.error.is_some(),
        "a shadowed cast root must not return imported references"
    );
    server.shutdown();
}

#[test]
fn self_contained_local_references_ignore_unrelated_broken_imports() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let source_path = root.join("LocalReferences.pas");
    let broken_import = root.join("BrokenImport.pas");
    let broken_include = root.join("BrokenImport.inc");
    let source = "unit LocalReferences;\ninterface\nuses BrokenImport, MissingSdkUnit;\nimplementation\nprocedure Run;\nvar\n  LocalValue: Integer;\nbegin\n  LocalValue := 1;\n  Log(LocalValue);\n  LocalValue := 2;\nend;\nend.\n";
    write_file(&source_path, source);
    write_file(
        &broken_import,
        "unit BrokenImport;\ninterface\n{$I BrokenImport.inc}\nimplementation\nend.\n",
    );
    write_file(&broken_include, "{$IFDEF NEVER_DEFINED}\n");

    let mut server = TestServer::launch();
    server.initialize(root, Value::Null);

    let references_without_declaration =
        RequestId::from("local-references-without-declaration".to_string());
    server.send_request(
        references_without_declaration.clone(),
        "textDocument/references",
        json!({
            "textDocument": {"uri": uri(&source_path)},
            "position": position_of(source, "LocalValue", 0),
            "context": {"includeDeclaration": false}
        }),
    );
    let response = server.response(&references_without_declaration);
    let references_without_declaration = result_locations(response);
    assert_exact_location_signatures(
        &references_without_declaration,
        (1..=3)
            .map(|occurrence| {
                expected_location_signature(&source_path, source, "LocalValue", occurrence)
            })
            .collect(),
    );

    let references_with_declaration =
        RequestId::from("local-references-with-declaration".to_string());
    server.send_request(
        references_with_declaration.clone(),
        "textDocument/references",
        json!({
            "textDocument": {"uri": uri(&source_path)},
            "position": position_of(source, "LocalValue", 0),
            "context": {"includeDeclaration": true}
        }),
    );
    let response = server.response(&references_with_declaration);
    let references_with_declaration = result_locations(response);
    assert_exact_location_signatures(
        &references_with_declaration,
        (0..=3)
            .map(|occurrence| {
                expected_location_signature(&source_path, source, "LocalValue", occurrence)
            })
            .collect(),
    );

    let highlights_id = RequestId::from("local-highlights".to_string());
    server.send_request(
        highlights_id.clone(),
        "textDocument/documentHighlight",
        json!({
            "textDocument": {"uri": uri(&source_path)},
            "position": position_of(source, "LocalValue", 0)
        }),
    );
    let highlights = server.response(&highlights_id);
    assert!(
        highlights.error.is_none(),
        "local highlights failed: {highlights:?}"
    );
    let highlights = highlights
        .result
        .expect("local highlights result")
        .as_array()
        .expect("local highlights array")
        .clone();
    let mut actual = highlights.iter().map(range_signature).collect::<Vec<_>>();
    actual.sort();
    let mut expected = (0..=3)
        .map(|occurrence| {
            let (_, line, start, _, end) =
                expected_location_signature(&source_path, source, "LocalValue", occurrence);
            (line, start, line, end)
        })
        .collect::<Vec<_>>();
    expected.sort();
    assert_eq!(actual, expected);

    server.shutdown();
}

#[test]
fn local_references_do_not_skip_imports_when_a_same_source_shadow_is_present() {
    let temp = tempfile::tempdir().unwrap();
    let source_path = temp.path().join("ShadowedLocalReferences.pas");
    let source = "unit ShadowedLocalReferences;\ninterface\nuses MissingSdkUnit;\nimplementation\nprocedure Run;\nvar\n  LocalValue: Integer;\n  procedure Nested;\n  var\n    LocalValue: Integer;\n  begin\n    LocalValue := 2;\n  end;\nbegin\n  LocalValue := 1;\n  Nested;\nend;\nend.\n";
    write_file(&source_path, source);

    let mut server = TestServer::launch();
    server.initialize(temp.path(), Value::Null);
    let id = RequestId::from("shadowed-local-references".to_string());
    server.send_request(
        id.clone(),
        "textDocument/references",
        json!({
            "textDocument": {"uri": uri(&source_path)},
            "position": position_of(source, "LocalValue", 0),
            "context": {"includeDeclaration": false}
        }),
    );
    let response = server.response(&id);
    let error = response
        .error
        .expect("same-source shadowing must not authorize skipping imports");
    assert_eq!(error.code, -32803);
    assert!(error.message.contains("incomplete"), "{error:?}");
    assert!(response.result.is_none());
    server.shutdown();
}

#[test]
fn references_preserve_nested_project_ownership_without_overrides() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path().join("workspace");
    let parent_main = root.join("Main.pas");
    let nested_project = root.join("nested/Nested.dproj");
    let consumer = root.join("nested/src/Consumer.pas");
    let provider = root.join("nested/lib/Provider.pas");
    let consumer_source = "unit Consumer;\ninterface\nuses Provider;\nimplementation\nprocedure Run;\nbegin\n  Log(SharedValue);\nend;\nend.\n";
    let provider_source =
        "unit Provider;\ninterface\nconst SharedValue = 1;\nimplementation\nend.\n";
    write_file(
        &parent_main,
        "unit Main;\ninterface\nimplementation\nend.\n",
    );
    write_file(&consumer, consumer_source);
    write_file(&provider, provider_source);
    write_file(
        &root.join("App.dproj"),
        "<Project><PropertyGroup><MainSource>Main.pas</MainSource></PropertyGroup></Project>",
    );
    write_file(
        &nested_project,
        "<Project><PropertyGroup><MainSource>src/Consumer.pas</MainSource><DCC_UnitSearchPath>lib</DCC_UnitSearchPath></PropertyGroup></Project>",
    );

    let mut server = TestServer::launch();
    server.initialize(&root, Value::Null);
    let id = RequestId::from("nested-project-ownership".to_string());
    server.send_request(
        id.clone(),
        "textDocument/references",
        json!({
            "textDocument": {"uri": uri(&provider)},
            "position": position_of(provider_source, "SharedValue", 0),
            "context": {"includeDeclaration": false}
        }),
    );
    let references = result_locations(server.response(&id));
    assert_eq!(references.len(), 1);
    assert_eq!(references[0]["uri"], uri(&consumer).to_string());
    server.shutdown();
}

#[test]
fn local_references_still_reject_an_unsafe_include_in_the_target_document() {
    let temp = tempfile::tempdir().unwrap();
    let source_path = temp.path().join("UnsafeLocalReferences.pas");
    let include_path = temp.path().join("Unsafe.inc");
    let source = "unit UnsafeLocalReferences;\ninterface\nimplementation\n{$I Unsafe.inc}\nprocedure Run;\nvar\n  LocalValue: Integer;\nbegin\n  LocalValue := 1;\nend;\nend.\n";
    write_file(&source_path, source);
    write_file(&include_path, "{$IFDEF NEVER_DEFINED}\n");

    let mut server = TestServer::launch();
    server.initialize(temp.path(), Value::Null);
    let id = RequestId::from("unsafe-target-include-references".to_string());
    server.send_request(
        id.clone(),
        "textDocument/references",
        json!({
            "textDocument": {"uri": uri(&source_path)},
            "position": position_of(source, "LocalValue", 0),
            "context": {"includeDeclaration": false}
        }),
    );
    let response = server.response(&id);
    let error = response
        .error
        .expect("an unsafe target include must fail closed");
    assert_eq!(error.code, -32803);
    assert!(error.message.contains("include"), "{error:?}");
    assert!(response.result.is_none());
    server.shutdown();
}

#[test]
fn self_contained_local_references_ignore_incomplete_project_context() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let source_path = root.join("src/LocalReferences.pas");
    let source = "unit LocalReferences;\ninterface\nuses MissingSdkUnit;\nimplementation\nprocedure Run;\nvar\n  LocalValue: Integer;\nbegin\n  LocalValue := 1;\n  Log(LocalValue);\n  LocalValue := 2;\nend;\nend.\n";
    write_file(&source_path, source);
    write_file(&root.join("One.dpr"), "program One; begin end.");
    write_file(&root.join("Two.dpr"), "program Two; begin end.");
    write_file(
        &root.join("One.dproj"),
        "<Project><PropertyGroup><MainSource>One.dpr</MainSource></PropertyGroup><ItemGroup><DCCReference Include=\"src/LocalReferences.pas\" /></ItemGroup></Project>",
    );
    write_file(
        &root.join("Two.dproj"),
        "<Project><PropertyGroup><MainSource>Two.dpr</MainSource></PropertyGroup><ItemGroup><DCCReference Include=\"src/LocalReferences.pas\" /></ItemGroup></Project>",
    );

    let mut server = TestServer::launch();
    server.initialize(root, Value::Null);

    let without_declaration_id =
        RequestId::from("incomplete-context-without-declaration".to_string());
    server.send_request(
        without_declaration_id.clone(),
        "textDocument/references",
        json!({
            "textDocument": {"uri": uri(&source_path)},
            "position": position_of(source, "LocalValue", 0),
            "context": {"includeDeclaration": false}
        }),
    );
    let without_declaration = result_locations(server.response(&without_declaration_id));
    assert_exact_location_signatures(
        &without_declaration,
        (1..=3)
            .map(|occurrence| {
                expected_location_signature(&source_path, source, "LocalValue", occurrence)
            })
            .collect(),
    );

    let with_declaration_id = RequestId::from("incomplete-context-with-declaration".to_string());
    server.send_request(
        with_declaration_id.clone(),
        "textDocument/references",
        json!({
            "textDocument": {"uri": uri(&source_path)},
            "position": position_of(source, "LocalValue", 0),
            "context": {"includeDeclaration": true}
        }),
    );
    let with_declaration = result_locations(server.response(&with_declaration_id));
    assert_exact_location_signatures(
        &with_declaration,
        (0..=3)
            .map(|occurrence| {
                expected_location_signature(&source_path, source, "LocalValue", occurrence)
            })
            .collect(),
    );

    server.shutdown();
}

#[test]
fn local_references_reject_project_dependent_conditional_bindings_in_incomplete_context() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let source_path = root.join("src/ConditionalReferences.pas");
    let source = "unit ConditionalReferences;\ninterface\nimplementation\nprocedure Run;\n{$IFDEF PROJECT_FEATURE}\nvar\n  LocalValue: Integer;\n{$ENDIF}\nbegin\n{$IFDEF PROJECT_FEATURE}\n  LocalValue := 1;\n  Log(LocalValue);\n  LocalValue := 2;\n{$ENDIF}\nend;\nend.\n";
    write_file(&source_path, source);
    write_file(&root.join("One.dpr"), "program One; begin end.");
    write_file(&root.join("Two.dpr"), "program Two; begin end.");
    write_file(
        &root.join("One.dproj"),
        "<Project><PropertyGroup><MainSource>One.dpr</MainSource><DCC_Define>PROJECT_FEATURE</DCC_Define></PropertyGroup><ItemGroup><DCCReference Include=\"src/ConditionalReferences.pas\" /></ItemGroup></Project>",
    );
    write_file(
        &root.join("Two.dproj"),
        "<Project><PropertyGroup><MainSource>Two.dpr</MainSource></PropertyGroup><ItemGroup><DCCReference Include=\"src/ConditionalReferences.pas\" /></ItemGroup></Project>",
    );

    let mut server = TestServer::launch();
    server.initialize(root, Value::Null);
    let id = RequestId::from("incomplete-conditional-local-reference".to_string());
    server.send_request(
        id.clone(),
        "textDocument/references",
        json!({
            "textDocument": {"uri": uri(&source_path)},
            "position": position_of(source, "LocalValue", 0),
            "context": {"includeDeclaration": false}
        }),
    );
    let response = server.response(&id);
    let error = response
        .error
        .expect("project-dependent conditional binding must fail closed");
    assert_eq!(error.code, -32803);
    assert!(
        error.message.contains("conditional") || error.message.contains("incomplete"),
        "unexpected conditional safety error: {error:?}"
    );
    assert!(response.result.is_none());
    server.shutdown();
}

#[test]
fn local_references_reject_unsafe_target_includes_in_incomplete_context() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let source_path = root.join("src/UnsafeReferences.pas");
    let include_path = root.join("src/Unsafe.inc");
    let source = "unit UnsafeReferences;\ninterface\nimplementation\n{$I Unsafe.inc}\nprocedure Run;\nvar\n  LocalValue: Integer;\nbegin\n  LocalValue := 1;\nend;\nend.\n";
    write_file(&source_path, source);
    write_file(&include_path, "{$IFDEF NEVER_DEFINED}\n");
    write_file(&root.join("One.dpr"), "program One; begin end.");
    write_file(&root.join("Two.dpr"), "program Two; begin end.");
    write_file(
        &root.join("One.dproj"),
        "<Project><PropertyGroup><MainSource>One.dpr</MainSource></PropertyGroup><ItemGroup><DCCReference Include=\"src/UnsafeReferences.pas\" /></ItemGroup></Project>",
    );
    write_file(
        &root.join("Two.dproj"),
        "<Project><PropertyGroup><MainSource>Two.dpr</MainSource></PropertyGroup><ItemGroup><DCCReference Include=\"src/UnsafeReferences.pas\" /></ItemGroup></Project>",
    );

    let mut server = TestServer::launch();
    server.initialize(root, Value::Null);
    let id = RequestId::from("incomplete-unsafe-target-include".to_string());
    server.send_request(
        id.clone(),
        "textDocument/references",
        json!({
            "textDocument": {"uri": uri(&source_path)},
            "position": position_of(source, "LocalValue", 0),
            "context": {"includeDeclaration": false}
        }),
    );
    let response = server.response(&id);
    let error = response
        .error
        .expect("unsafe target include must fail closed");
    assert_eq!(error.code, -32803);
    assert!(
        error.message.contains("include"),
        "unexpected include error: {error:?}"
    );
    assert!(response.result.is_none());
    server.shutdown();
}

#[test]
fn local_references_do_not_use_incomplete_project_include_paths() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let source_path = root.join("src/IncludePathReferences.pas");
    let source = "unit IncludePathReferences;\ninterface\nimplementation\n{$I Config.inc}\nprocedure Run;\nvar\n  LocalValue: Integer;\nbegin\n  LocalValue := 1;\nend;\nend.\n";
    write_file(&source_path, source);
    write_file(&root.join("Config.inc"), "{$DEFINE HARMLESS}\n");
    write_file(
        &root.join("unsafe-one/Config.inc"),
        "{$IFDEF PROJECT_UNSAFE}\n",
    );
    write_file(
        &root.join("unsafe-two/Config.inc"),
        "{$IFDEF PROJECT_UNSAFE}\n",
    );
    write_file(&root.join("One.dpr"), "program One; begin end.");
    write_file(&root.join("Two.dpr"), "program Two; begin end.");
    write_file(
        &root.join("One.dproj"),
        "<Project><PropertyGroup><MainSource>One.dpr</MainSource><DCC_IncludePath>unsafe-one</DCC_IncludePath></PropertyGroup><ItemGroup><DCCReference Include=\"src/IncludePathReferences.pas\" /></ItemGroup></Project>",
    );
    write_file(
        &root.join("Two.dproj"),
        "<Project><PropertyGroup><MainSource>Two.dpr</MainSource><DCC_IncludePath>unsafe-two</DCC_IncludePath></PropertyGroup><ItemGroup><DCCReference Include=\"src/IncludePathReferences.pas\" /></ItemGroup></Project>",
    );

    let mut server = TestServer::launch();
    server.initialize(root, Value::Null);
    let id = RequestId::from("incomplete-project-include-paths".to_string());
    server.send_request(
        id.clone(),
        "textDocument/references",
        json!({
            "textDocument": {"uri": uri(&source_path)},
            "position": position_of(source, "LocalValue", 0),
            "context": {"includeDeclaration": false}
        }),
    );
    let response = server.response(&id);
    let error = response
        .error
        .expect("incomplete project include paths must not be bypassed");
    assert_eq!(error.code, -32803);
    assert!(
        error.message.contains("include"),
        "unexpected include error: {error:?}"
    );
    assert!(response.result.is_none());
    server.shutdown();
}

#[test]
fn self_contained_local_references_allow_known_inactive_target_includes() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let source_path = root.join("src/InactiveIncludeReferences.pas");
    let source = "unit InactiveIncludeReferences;\ninterface\nuses MissingSdkUnit;\nimplementation\n{$IF 1 = 0}\n{$I MissingConfig.inc}\n{$ENDIF}\nprocedure Run;\nvar\n  LocalValue: Integer;\nbegin\n  LocalValue := 1;\nend;\nend.\n";
    write_file(&source_path, source);
    write_file(&root.join("One.dpr"), "program One; begin end.");
    write_file(&root.join("Two.dpr"), "program Two; begin end.");
    write_file(
        &root.join("One.dproj"),
        "<Project><PropertyGroup><MainSource>One.dpr</MainSource></PropertyGroup><ItemGroup><DCCReference Include=\"src/InactiveIncludeReferences.pas\" /></ItemGroup></Project>",
    );
    write_file(
        &root.join("Two.dproj"),
        "<Project><PropertyGroup><MainSource>Two.dpr</MainSource></PropertyGroup><ItemGroup><DCCReference Include=\"src/InactiveIncludeReferences.pas\" /></ItemGroup></Project>",
    );

    let mut server = TestServer::launch();
    server.initialize(root, Value::Null);
    let id = RequestId::from("incomplete-known-inactive-include".to_string());
    server.send_request(
        id.clone(),
        "textDocument/references",
        json!({
            "textDocument": {"uri": uri(&source_path)},
            "position": position_of(source, "LocalValue", 0),
            "context": {"includeDeclaration": false}
        }),
    );
    let references = result_locations(server.response(&id));
    assert_exact_location_signatures(
        &references,
        std::iter::once(expected_location_signature(
            &source_path,
            source,
            "LocalValue",
            1,
        ))
        .collect(),
    );
    server.shutdown();
}

#[test]
fn nonlocal_references_remain_blocked_by_incomplete_project_context() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let source_path = root.join("src/ImportedReferences.pas");
    let provider_path = root.join("src/Provider.pas");
    let source = "unit ImportedReferences;\ninterface\nuses Provider;\nimplementation\nprocedure Run;\nbegin\n  Log(SharedValue);\nend;\nend.\n";
    write_file(&source_path, source);
    write_file(
        &provider_path,
        "unit Provider;\ninterface\nconst SharedValue = 1;\nimplementation\nend.\n",
    );
    write_file(&root.join("One.dpr"), "program One; begin end.");
    write_file(&root.join("Two.dpr"), "program Two; begin end.");
    write_file(
        &root.join("One.dproj"),
        "<Project><PropertyGroup><MainSource>One.dpr</MainSource></PropertyGroup><ItemGroup><DCCReference Include=\"src/ImportedReferences.pas\" /><DCCReference Include=\"src/Provider.pas\" /></ItemGroup></Project>",
    );
    write_file(
        &root.join("Two.dproj"),
        "<Project><PropertyGroup><MainSource>Two.dpr</MainSource></PropertyGroup><ItemGroup><DCCReference Include=\"src/ImportedReferences.pas\" /><DCCReference Include=\"src/Provider.pas\" /></ItemGroup></Project>",
    );

    let mut server = TestServer::launch();
    server.initialize(root, Value::Null);
    let id = RequestId::from("incomplete-nonlocal-reference".to_string());
    server.send_request(
        id.clone(),
        "textDocument/references",
        json!({
            "textDocument": {"uri": uri(&source_path)},
            "position": position_of(source, "SharedValue", 0),
            "context": {"includeDeclaration": false}
        }),
    );
    let response = server.response(&id);
    let error = response
        .error
        .expect("nonlocal reference must not bypass incomplete project context");
    assert_eq!(error.code, -32803);
    assert!(
        error.message.contains("incomplete"),
        "unexpected nonlocal error: {error:?}"
    );
    assert!(response.result.is_none());
    server.shutdown();
}

#[test]
fn explicit_external_unit_keeps_legacy_sibling_lookup_without_configuration() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let project_root = temp.path().join("project");
    let external_root = temp.path().join("external");
    let main = project_root.join("Main.pas");
    let project = project_root.join("App.dproj");
    let external = external_root.join("External.pas");
    let helper = external_root.join("Helper.pas");
    let main_source = "unit Main;\ninterface\nuses External;\nimplementation\nprocedure Run;\nbegin\n  ExternalRoutine;\nend;\nend.\n";
    let external_source = "unit External;\ninterface\nuses Helper;\nprocedure ExternalRoutine;\nimplementation\nprocedure ExternalRoutine;\nbegin\n  HelperRoutine;\nend;\nend.\n";
    let helper_source = "unit Helper;\ninterface\nprocedure HelperRoutine;\nimplementation\nprocedure HelperRoutine; begin end;\nend.\n";
    write_file(&main, main_source);
    write_file(&external, external_source);
    write_file(&helper, helper_source);
    write_file(
        &project,
        "<Project><PropertyGroup><MainSource>Main.pas</MainSource></PropertyGroup><ItemGroup><DCCReference Include=\"../external/External.pas\" /></ItemGroup></Project>",
    );

    let mut server = TestServer::launch();
    server.initialize(&project_root, Value::Null);
    let external_id = RequestId::from("explicit-external-unit".to_string());
    server.send_request(
        external_id.clone(),
        "textDocument/definition",
        navigation_params(&main, main_source, "ExternalRoutine", 0),
    );
    let external_locations = result_locations(server.response(&external_id));
    assert_eq!(external_locations.len(), 1);
    assert_eq!(external_locations[0]["uri"], uri(&external).to_string());

    let helper_id = RequestId::from("explicit-external-sibling".to_string());
    server.send_request(
        helper_id.clone(),
        "textDocument/definition",
        navigation_params(&external, external_source, "HelperRoutine", 0),
    );
    let helper_locations = result_locations(server.response(&helper_id));
    assert_eq!(helper_locations.len(), 1);
    assert_eq!(helper_locations[0]["uri"], uri(&helper).to_string());
    server.shutdown();
}

#[test]
fn legacy_sibling_route_survives_closed_assistance_and_disk_refresh() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let project_root = temp.path().join("project");
    let external_root = temp.path().join("external");
    let main = project_root.join("Main.pas");
    let project = project_root.join("App.dproj");
    let external = external_root.join("External.pas");
    let helper = external_root.join("Helper.pas");
    let third = external_root.join("Third.pas");
    let main_source = "unit Main;\ninterface\nuses External;\nimplementation\nprocedure Run;\nbegin\n  ExternalRoutine;\nend;\nend.\n";
    let external_source = "unit External;\ninterface\nuses Helper;\nprocedure ExternalRoutine;\nimplementation\nprocedure ExternalRoutine;\nbegin\n  HelperRoutine;\nend;\nend.\n";
    let helper_source = "unit Helper;\ninterface\nuses Third;\nprocedure HelperRoutine;\nimplementation\nprocedure HelperRoutine;\nbegin\n  ThirdRoutine;\nend;\nend.\n";
    let third_source = "unit Third;\ninterface\nprocedure ThirdRoutine;\nimplementation\nprocedure ThirdRoutine; begin end;\nend.\n";
    write_file(&main, main_source);
    write_file(&external, external_source);
    write_file(&helper, helper_source);
    write_file(&third, third_source);
    write_file(
        &project,
        "<Project><PropertyGroup><MainSource>Main.pas</MainSource></PropertyGroup><ItemGroup><DCCReference Include=\"../external/External.pas\" /></ItemGroup></Project>",
    );

    let mut server = TestServer::launch();
    server.initialize(&project_root, Value::Null);

    let external_id = RequestId::from("legacy-route-external".to_string());
    server.send_request(
        external_id.clone(),
        "textDocument/definition",
        navigation_params(&main, main_source, "ExternalRoutine", 0),
    );
    assert_eq!(
        result_locations(server.response(&external_id))[0]["uri"],
        uri(&external).to_string()
    );

    let helper_id = RequestId::from("legacy-route-helper".to_string());
    server.send_request(
        helper_id.clone(),
        "textDocument/definition",
        navigation_params(&external, external_source, "HelperRoutine", 0),
    );
    assert_eq!(
        result_locations(server.response(&helper_id))[0]["uri"],
        uri(&helper).to_string()
    );

    let hover_id = RequestId::from("legacy-route-hover".to_string());
    server.send_request(
        hover_id.clone(),
        "textDocument/hover",
        navigation_params(&helper, helper_source, "ThirdRoutine", 0),
    );
    let hover = server.response(&hover_id);
    assert!(
        hover.error.is_none(),
        "closed helper hover failed: {hover:?}"
    );
    assert!(
        hover
            .result
            .as_ref()
            .is_some_and(|result| !result.is_null()),
        "closed helper hover lost its legacy route: {hover:?}"
    );

    let references_id = RequestId::from("legacy-route-references".to_string());
    server.send_request(
        references_id.clone(),
        "textDocument/references",
        json!({
            "textDocument": {"uri": uri(&helper)},
            "position": position_of(helper_source, "ThirdRoutine", 0),
            "context": {"includeDeclaration": false}
        }),
    );
    let references = server.response(&references_id);
    assert!(
        references.error.is_none(),
        "closed helper reference preflight failed: {references:?}"
    );
    assert!(
        references
            .result
            .as_ref()
            .and_then(Value::as_array)
            .is_some_and(|locations| {
                locations
                    .iter()
                    .any(|location| location["uri"] == uri(&helper).to_string())
            }),
        "closed helper references lost their legacy route: {references:?}"
    );

    fs::write(&helper, format!("{helper_source}\n")).expect("refresh helper source");
    server.send_notification(
        "workspace/didChangeWatchedFiles",
        json!({"changes": [{"uri": uri(&helper), "type": 2}]}),
    );

    let refreshed_id = RequestId::from("legacy-route-refresh".to_string());
    server.send_request(
        refreshed_id.clone(),
        "textDocument/definition",
        navigation_params(&helper, &format!("{helper_source}\n"), "ThirdRoutine", 0),
    );
    assert_eq!(
        result_locations(server.response(&refreshed_id))[0]["uri"],
        uri(&third).to_string(),
        "a refreshed closed helper must retain its verified legacy route"
    );
    server.shutdown();
}

#[test]
fn legacy_sibling_route_is_invalidated_when_owner_metadata_removes_the_reference() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let project_root = temp.path().join("project");
    let external_root = temp.path().join("external");
    let main = project_root.join("Main.pas");
    let project = project_root.join("App.dproj");
    let external = external_root.join("External.pas");
    let helper = external_root.join("Helper.pas");
    let third = external_root.join("Third.pas");
    let main_source = "unit Main;\ninterface\nuses External;\nimplementation\nprocedure Run;\nbegin\n  ExternalRoutine;\nend;\nend.\n";
    let external_source = "unit External;\ninterface\nuses Helper;\nprocedure ExternalRoutine;\nimplementation\nprocedure ExternalRoutine;\nbegin\n  HelperRoutine;\nend;\nend.\n";
    let helper_source = "unit Helper;\ninterface\nuses Third;\nprocedure HelperRoutine;\nimplementation\nprocedure HelperRoutine;\nbegin\n  ThirdRoutine;\nend;\nend.\n";
    let third_source = "unit Third;\ninterface\nprocedure ThirdRoutine;\nimplementation\nprocedure ThirdRoutine; begin end;\nend.\n";
    write_file(&main, main_source);
    write_file(&external, external_source);
    write_file(&helper, helper_source);
    write_file(&third, third_source);
    write_file(
        &project,
        "<Project><PropertyGroup><MainSource>Main.pas</MainSource></PropertyGroup><ItemGroup><DCCReference Include=\"../external/External.pas\" /></ItemGroup></Project>",
    );

    let mut server = TestServer::launch();
    server.initialize(&project_root, Value::Null);
    let external_id = RequestId::from("metadata-route-external".to_string());
    server.send_request(
        external_id.clone(),
        "textDocument/definition",
        navigation_params(&main, main_source, "ExternalRoutine", 0),
    );
    assert_eq!(
        result_locations(server.response(&external_id))[0]["uri"],
        uri(&external).to_string()
    );
    let helper_id = RequestId::from("metadata-route-helper".to_string());
    server.send_request(
        helper_id.clone(),
        "textDocument/definition",
        navigation_params(&external, external_source, "HelperRoutine", 0),
    );
    assert_eq!(
        result_locations(server.response(&helper_id))[0]["uri"],
        uri(&helper).to_string()
    );

    write_file(
        &project,
        "<Project><PropertyGroup><MainSource>Main.pas</MainSource></PropertyGroup></Project>",
    );
    server.send_notification(
        "workspace/didChangeWatchedFiles",
        json!({"changes": [{"uri": uri(&project), "type": 2}]}),
    );

    let third_id = RequestId::from("metadata-route-third".to_string());
    server.send_request(
        third_id.clone(),
        "textDocument/definition",
        navigation_params(&helper, helper_source, "ThirdRoutine", 0),
    );
    let response = server.response(&third_id);
    assert!(
        response.error.is_none(),
        "metadata refresh failed: {response:?}"
    );
    assert!(
        result_locations(response).is_empty(),
        "removing the explicit owner reference must invalidate the legacy route"
    );
    server.shutdown();
}

#[test]
fn legacy_explicit_dependency_overlay_survives_deleted_backing_file_until_close() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let workspace_root = temp.path().join("workspace");
    let project_root = workspace_root.clone();
    let main = project_root.join("Main.pas");
    let project = workspace_root.join("App.dproj");
    let competing_project = workspace_root.join("Other.dproj");
    let provider = workspace_root.join("Provider.pas");
    let main_source = "unit Main;\ninterface\nuses Provider;\nimplementation\nprocedure Run;\nbegin\n  SharedRoutine;\nend;\nend.\n";
    let provider_source = "unit Provider;\ninterface\nprocedure SharedRoutine;\nimplementation\nprocedure SharedRoutine; begin end;\nend.\n";
    write_file(&main, main_source);
    write_file(&provider, provider_source);
    write_file(
        &project,
        "<Project><PropertyGroup><MainSource>Main.pas</MainSource></PropertyGroup><ItemGroup><DCCReference Include=\"Provider.pas\" /></ItemGroup></Project>",
    );

    let mut server = TestServer::launch();
    server.initialize(&workspace_root, Value::Null);
    let initial_context_id = RequestId::from("overlay-initial-context".to_string());
    server.send_request(
        initial_context_id.clone(),
        "pascal/projectContext",
        json!({"textDocument": {"uri": uri(&main)}}),
    );
    let initial_context = server.response(&initial_context_id);
    assert!(
        initial_context.error.is_none(),
        "initial project context failed: {initial_context:?}"
    );
    assert_eq!(
        initial_context.result.as_ref().unwrap()["selectionMode"],
        "automatic"
    );
    assert_eq!(
        initial_context.result.as_ref().unwrap()["selectedProjectUri"],
        uri(&project).to_string()
    );

    let initial_id = RequestId::from("overlay-initial-definition".to_string());
    server.send_request(
        initial_id.clone(),
        "textDocument/definition",
        navigation_params(&main, main_source, "SharedRoutine", 0),
    );
    let initial = result_locations(server.response(&initial_id));
    assert_eq!(initial.len(), 1);
    assert_eq!(initial[0]["uri"], uri(&provider).to_string());

    server.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri(&provider),
                "languageId": "pascal",
                "version": 1,
                "text": provider_source
            }
        }),
    );
    let _ = server.notification("textDocument/publishDiagnostics");

    fs::remove_file(&provider).expect("delete provider backing file");

    let definition_id = RequestId::from("overlay-after-delete-definition".to_string());
    server.send_request(
        definition_id.clone(),
        "textDocument/definition",
        navigation_params(&main, main_source, "SharedRoutine", 0),
    );
    let definition = result_locations(server.response(&definition_id));
    assert_eq!(definition.len(), 1);
    assert_eq!(definition[0]["uri"], uri(&provider).to_string());

    let hover_id = RequestId::from("overlay-after-delete-hover".to_string());
    server.send_request(
        hover_id.clone(),
        "textDocument/hover",
        navigation_params(&main, main_source, "SharedRoutine", 0),
    );
    let hover = server.response(&hover_id);
    assert!(hover.error.is_none(), "overlay hover failed: {hover:?}");
    assert!(
        hover
            .result
            .as_ref()
            .is_some_and(|result| !result.is_null()),
        "overlay hover lost the imported provider: {hover:?}"
    );

    write_file(
        &competing_project,
        "<Project><PropertyGroup><MainSource>Main.pas</MainSource></PropertyGroup></Project>",
    );
    let competing_definition_id = RequestId::from("overlay-after-competing-project".to_string());
    server.send_request(
        competing_definition_id.clone(),
        "textDocument/definition",
        navigation_params(&main, main_source, "SharedRoutine", 0),
    );
    let competing_definition = server.response(&competing_definition_id);
    assert!(
        competing_definition.error.is_none(),
        "navigation with the open overlay failed: {competing_definition:?}"
    );

    let competing_context_id = RequestId::from("overlay-competing-context".to_string());
    server.send_request(
        competing_context_id.clone(),
        "pascal/projectContext",
        json!({"textDocument": {"uri": uri(&main)}}),
    );
    let competing_context = server.response(&competing_context_id);
    assert!(
        competing_context.error.is_none(),
        "competing project context failed: {competing_context:?}"
    );
    let competing_context_result = competing_context.result.as_ref().unwrap();
    assert_eq!(
        competing_context_result["selectionMode"], "ambiguous",
        "new competing project must not leave the old automatic selection cached: {competing_context_result}"
    );
    assert!(
        competing_context_result["selectedProjectUri"].is_null(),
        "ambiguous context must not retain App.dproj: {competing_context_result}"
    );

    server.send_notification(
        "textDocument/didClose",
        json!({"textDocument": {"uri": uri(&provider)}}),
    );
    let missing_id = RequestId::from("overlay-after-close-missing".to_string());
    server.send_request(
        missing_id.clone(),
        "textDocument/definition",
        navigation_params(&main, main_source, "SharedRoutine", 0),
    );
    let missing = server.response(&missing_id);
    assert!(
        missing.error.is_none(),
        "missing provider failed: {missing:?}"
    );
    assert_eq!(
        missing.result,
        Some(Value::Array(Vec::new())),
        "closing the overlay must expose the missing backing file"
    );
    server.shutdown();
}

#[cfg(target_os = "linux")]
#[test]
fn explicit_native_symlink_leaf_and_ancestor_are_followed() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let project_root = temp.path().join("project");
    let external_root = temp.path().join("external");
    let ancestor_target = external_root.join("ancestor-target");
    let ancestor_link = temp.path().join("ancestor-link");
    let main = project_root.join("Main.pas");
    let project = project_root.join("App.dproj");
    let leaf = external_root.join("Leaf.pas");
    let leaf_target = external_root.join("LeafTarget.pas");
    let ancestor = ancestor_link.join("Ancestor.pas");
    let ancestor_target_file = ancestor_target.join("Ancestor.pas");
    let main_source = "unit Main;\ninterface\nuses Leaf, Ancestor;\nimplementation\nprocedure Run;\nbegin\n  LeafRoutine;\n  AncestorRoutine;\nend;\nend.\n";
    let leaf_source = "unit Leaf;\ninterface\nprocedure LeafRoutine;\nimplementation\nprocedure LeafRoutine; begin end;\nend.\n";
    let ancestor_source = "unit Ancestor;\ninterface\nprocedure AncestorRoutine;\nimplementation\nprocedure AncestorRoutine; begin end;\nend.\n";
    write_file(&main, main_source);
    write_file(&leaf_target, leaf_source);
    write_file(&ancestor_target_file, ancestor_source);
    fs::create_dir_all(&ancestor_target).expect("ancestor target directory");
    symlink(&leaf_target, &leaf).expect("leaf symlink");
    symlink(&ancestor_target, &ancestor_link).expect("ancestor symlink");
    write_file(
        &project,
        "<Project><PropertyGroup><MainSource>Main.pas</MainSource></PropertyGroup><ItemGroup><DCCReference Include=\"../external/Leaf.pas\" /><DCCReference Include=\"../ancestor-link/Ancestor.pas\" /></ItemGroup></Project>",
    );

    let mut leaf_server = TestServer::launch();
    leaf_server.initialize(&project_root, Value::Null);
    let leaf_opened = observed_open(&leaf_target, || {
        let id = RequestId::from("native-leaf-symlink".to_string());
        leaf_server.send_request(
            id.clone(),
            "textDocument/definition",
            navigation_params(&main, main_source, "LeafRoutine", 0),
        );
        let response = leaf_server.response(&id);
        let locations = result_locations(response);
        assert_eq!(locations.len(), 1);
        assert_eq!(locations[0]["uri"], uri(&leaf).to_string());
    });
    assert!(leaf_opened, "explicit native leaf symlink was not followed");
    leaf_server.shutdown();

    let mut ancestor_server = TestServer::launch();
    ancestor_server.initialize(&project_root, Value::Null);
    let ancestor_opened = observed_open(&ancestor_target_file, || {
        let id = RequestId::from("native-ancestor-symlink".to_string());
        ancestor_server.send_request(
            id.clone(),
            "textDocument/definition",
            navigation_params(&main, main_source, "AncestorRoutine", 0),
        );
        let response = ancestor_server.response(&id);
        let locations = result_locations(response);
        assert_eq!(locations.len(), 1);
        assert_eq!(locations[0]["uri"], uri(&ancestor).to_string());
    });
    assert!(
        ancestor_opened,
        "explicit native ancestor symlink was not followed"
    );
    ancestor_server.shutdown();
}

#[cfg(target_os = "linux")]
fn assert_late_legacy_symlink_dependency_is_not_editable(ancestor_symlink: bool) {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let project_root = temp.path().join("project");
    let outside_root = temp.path().join("outside");
    let main = project_root.join("Main.pas");
    let project = project_root.join("App.dproj");
    let main_source = "unit Main;\ninterface\nuses Provider;\nimplementation\nprocedure Run;\nbegin\n  SharedValue;\nend;\nend.\n";
    let provider_source =
        "unit Provider;\ninterface\nconst SharedValue = 1;\nimplementation\nend.\n";
    fs::create_dir_all(&project_root).expect("project directory");
    let (provider_link, provider_target, reference) = if ancestor_symlink {
        let target_directory = outside_root.join("sdk");
        let link = project_root.join("sdk-link");
        let target = target_directory.join("Provider.pas");
        fs::create_dir_all(&target_directory).expect("provider target directory");
        symlink(&target_directory, &link).expect("ancestor symlink");
        (link.join("Provider.pas"), target, "sdk-link/Provider.pas")
    } else {
        let target = outside_root.join("Provider.pas");
        let link = project_root.join("Provider.pas");
        write_file(&target, provider_source);
        symlink(&target, &link).expect("leaf symlink");
        (link, target, "Provider.pas")
    };
    write_file(&main, main_source);
    write_file(&provider_target, provider_source);
    write_file(
        &project,
        &format!(
            "<Project><PropertyGroup><MainSource>Main.pas</MainSource></PropertyGroup><ItemGroup><DCCReference Include=\"{reference}\" /></ItemGroup></Project>"
        ),
    );

    let mut server = TestServer::launch();
    server.initialize(&project_root, Value::Null);
    let definition_id = RequestId::from("symlink-late-definition".to_string());
    server.send_request(
        definition_id.clone(),
        "textDocument/definition",
        navigation_params(&main, main_source, "SharedValue", 0),
    );
    let definition = result_locations(server.response(&definition_id));
    assert_eq!(definition.len(), 1);
    assert_eq!(definition[0]["uri"], uri(&provider_link).to_string());

    let rename_id = RequestId::from("symlink-late-rename".to_string());
    server.send_request(
        rename_id.clone(),
        "textDocument/rename",
        json!({
            "textDocument": {"uri": uri(&main)},
            "position": position_of(main_source, "SharedValue", 0),
            "newName": "RenamedValue"
        }),
    );
    let rename = server.response(&rename_id);
    let error = rename
        .error
        .expect("rename must reject edits through a native symlink");
    assert!(
        error.message.contains("outside configured workspace roots"),
        "unexpected symlink editability error: {error:?}"
    );
    assert!(rename.result.is_none(), "symlink dependency received edits");
    server.shutdown();
}

#[cfg(target_os = "linux")]
#[test]
fn rename_from_a_regular_importer_does_not_edit_a_late_leaf_symlink_dependency() {
    assert_late_legacy_symlink_dependency_is_not_editable(false);
}

#[cfg(target_os = "linux")]
#[test]
fn rename_from_a_regular_importer_does_not_edit_a_late_ancestor_symlink_dependency() {
    assert_late_legacy_symlink_dependency_is_not_editable(true);
}

#[cfg(target_os = "linux")]
#[test]
fn legacy_search_path_ancestor_symlink_is_followed_for_navigation() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let project_root = temp.path().join("project");
    let sdk_target = temp.path().join("sdk");
    let sdk_link = project_root.join("sdk-link");
    let main = project_root.join("Main.pas");
    let provider_target = sdk_target.join("Provider.pas");
    let main_source = "unit Main;\ninterface\nuses Provider;\nimplementation\nprocedure Run;\nbegin\n  ProviderRoutine;\nend;\nend.\n";
    let provider_source = "unit Provider;\ninterface\nprocedure ProviderRoutine;\nimplementation\nprocedure ProviderRoutine; begin end;\nend.\n";
    write_file(&main, main_source);
    write_file(&provider_target, provider_source);
    symlink(&sdk_target, &sdk_link).expect("legacy search path ancestor symlink");
    write_file(
        &project_root.join("App.dproj"),
        "<Project><PropertyGroup><MainSource>Main.pas</MainSource><DCC_UnitSearchPath>sdk-link</DCC_UnitSearchPath></PropertyGroup></Project>",
    );

    let mut server = TestServer::launch();
    server.initialize(&project_root, Value::Null);
    let opened = observed_open(&provider_target, || {
        let id = RequestId::from("legacy-search-symlink-definition".to_string());
        server.send_request(
            id.clone(),
            "textDocument/definition",
            navigation_params(&main, main_source, "ProviderRoutine", 0),
        );
        let locations = result_locations(server.response(&id));
        assert_eq!(locations.len(), 1);
        assert_eq!(
            locations[0]["uri"],
            uri(&sdk_link.join("Provider.pas")).to_string()
        );
    });
    assert!(opened, "legacy search path symlink target was not opened");
    server.shutdown();
}

#[cfg(target_os = "linux")]
fn assert_search_path_ancestor_symlink_is_not_opened(mapped: bool) {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let project_root = temp.path().join("project");
    let sdk_target = temp.path().join("sdk");
    let sdk_link = project_root.join("sdk-link");
    let main = project_root.join("Main.pas");
    let provider_target = sdk_target.join("Provider.pas");
    let main_source = "unit Main;\ninterface\nuses Provider;\nimplementation\nprocedure Run;\nbegin\n  ProviderRoutine;\nend;\nend.\n";
    let provider_source = "unit Provider;\ninterface\nprocedure ProviderRoutine;\nimplementation\nprocedure ProviderRoutine; begin end;\nend.\n";
    write_file(&main, main_source);
    write_file(&provider_target, provider_source);
    symlink(&sdk_target, &sdk_link).expect("configured search path ancestor symlink");
    write_file(
        &project_root.join("App.dproj"),
        if mapped {
            "<Project><PropertyGroup><MainSource>Main.pas</MainSource><DCC_UnitSearchPath>C:\\SDK\\sdk-link</DCC_UnitSearchPath></PropertyGroup></Project>"
        } else {
            "<Project><PropertyGroup><MainSource>Main.pas</MainSource></PropertyGroup></Project>"
        },
    );
    if mapped {
        write_file(
            &project_root.join(".delphi-tools.local.toml"),
            &format!(
                "[[path_mappings]]\nfrom = 'C:\\SDK'\nto = '{}'\n",
                project_root.display()
            ),
        );
    } else {
        write_file(
            &project_root.join(".delphi-tools.local.toml"),
            "[properties]\nDCC_UnitSearchPath = 'sdk-link'\n",
        );
    }

    let opened = observed_open(&provider_target, || {
        let mut server = TestServer::launch();
        server.initialize(&project_root, Value::Null);
        let id = RequestId::from("strict-search-symlink-definition".to_string());
        server.send_request(
            id.clone(),
            "textDocument/definition",
            navigation_params(&main, main_source, "ProviderRoutine", 0),
        );
        let response = server.response(&id);
        assert!(
            response.error.is_none(),
            "strict symlink lookup failed: {response:?}"
        );
        assert_eq!(
            response.result,
            Some(Value::Array(Vec::new())),
            "strict search path symlink target was opened"
        );
        server.shutdown();
    });
    assert!(!opened, "strict search path symlink target was opened");
}

#[cfg(target_os = "linux")]
#[test]
fn mapped_search_path_ancestor_symlink_is_not_opened() {
    assert_search_path_ancestor_symlink_is_not_opened(true);
}

#[cfg(target_os = "linux")]
#[test]
fn configured_search_path_ancestor_symlink_is_not_opened() {
    assert_search_path_ancestor_symlink_is_not_opened(false);
}

#[cfg(target_os = "linux")]
#[test]
fn configured_native_symlink_reference_is_not_opened_through_legacy_sibling_lookup() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let project_root = temp.path().join("project");
    let outside = temp.path().join("outside");
    let main = project_root.join("Main.pas");
    let provider = project_root.join("Provider.pas");
    let outside_provider = outside.join("Provider.pas");
    let main_source = "unit Main;\ninterface\nuses Provider;\nimplementation\nprocedure Run;\nbegin\n  SharedRoutine;\nend;\nend.\n";
    let provider_source = "unit Provider;\ninterface\nprocedure SharedRoutine;\nimplementation\nprocedure SharedRoutine; begin end;\nend.\n";
    write_file(&main, main_source);
    write_file(&outside_provider, provider_source);
    symlink(&outside_provider, &provider).expect("configured provider symlink");
    write_file(
        &project_root.join("App.dproj"),
        "<Project><PropertyGroup><MainSource>Main.pas</MainSource></PropertyGroup><ItemGroup><DCCReference Include=\"$(ProviderPath)\" /></ItemGroup></Project>",
    );
    write_file(
        &project_root.join(".delphi-tools.local.toml"),
        "[properties]\nProviderPath = 'Provider.pas'\n",
    );

    let opened = observed_open(&outside_provider, || {
        let mut server = TestServer::launch();
        server.initialize(&project_root, Value::Null);
        let id = RequestId::from("configured-symlink-safety".to_string());
        server.send_request(
            id.clone(),
            "textDocument/definition",
            navigation_params(&main, main_source, "SharedRoutine", 0),
        );
        let response = server.response(&id);
        assert!(
            response.error.is_none(),
            "configured symlink lookup failed: {response:?}"
        );
        assert_eq!(
            response.result,
            Some(Value::Array(Vec::new())),
            "configured symlink target was opened through the legacy sibling route"
        );
        server.shutdown();
    });
    assert!(!opened, "configured symlink target was opened");
}

#[test]
fn references_preserve_colocated_project_ownership_without_overrides() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path().join("workspace");
    let parent_main = root.join("0Main.pas");
    let consumer_a = root.join("A/src/Consumer.pas");
    let provider_a = root.join("A/lib/Provider.pas");
    let consumer_b = root.join("B/src/Consumer.pas");
    let provider_b = root.join("B/lib/Provider.pas");
    let consumer_a_source = "unit Consumer;\ninterface\nuses Provider;\nimplementation\nprocedure Run;\nbegin\n  Log(SharedA);\nend;\nend.\n";
    let consumer_b_source = "unit Consumer;\ninterface\nuses Provider;\nimplementation\nprocedure Run;\nbegin\n  Log(SharedB);\nend;\nend.\n";
    let provider_a_source = "unit Provider;\ninterface\nconst SharedA = 1;\nimplementation\nend.\n";
    let provider_b_source = "unit Provider;\ninterface\nconst SharedB = 1;\nimplementation\nend.\n";
    write_file(
        &parent_main,
        "unit Main;\ninterface\nimplementation\nend.\n",
    );
    write_file(&consumer_a, consumer_a_source);
    write_file(&provider_a, provider_a_source);
    write_file(&consumer_b, consumer_b_source);
    write_file(&provider_b, provider_b_source);
    for (directory, main_source) in [("A", "src/Consumer.pas"), ("B", "src/Consumer.pas")] {
        write_file(
            &root.join(directory).join("App.dproj"),
            &format!(
                "<Project><PropertyGroup><MainSource>{main_source}</MainSource><DCC_UnitSearchPath>lib</DCC_UnitSearchPath></PropertyGroup></Project>"
            ),
        );
    }
    write_file(
        &root.join("App.dproj"),
        "<Project><PropertyGroup><MainSource>0Main.pas</MainSource></PropertyGroup></Project>",
    );

    let mut server = TestServer::launch();
    server.initialize(&root, Value::Null);
    let id = RequestId::from("colocated-project-ownership".to_string());
    server.send_request(
        id.clone(),
        "textDocument/references",
        json!({
            "textDocument": {"uri": uri(&provider_a)},
            "position": position_of(provider_a_source, "SharedA", 0),
            "context": {"includeDeclaration": false}
        }),
    );
    let references = result_locations(server.response(&id));
    assert_eq!(references.len(), 1);
    assert_eq!(references[0]["uri"], uri(&consumer_a).to_string());
    server.shutdown();
}

#[cfg(unix)]
#[test]
fn references_include_an_unopened_consumer_from_a_mapped_root() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let project_root = temp.path().join("project");
    let sdk = temp.path().join("sdk");
    let main = project_root.join("Main.pas");
    let provider = sdk.join("Provider.pas");
    let consumer = sdk.join("Consumer.pas");
    let main_source = "unit Main;\ninterface\nuses Provider;\nimplementation\nprocedure Run;\nbegin\n  Log(SharedValue);\nend;\nend.\n";
    let provider_source =
        "unit Provider;\ninterface\nconst SharedValue = 1;\nimplementation\nend.\n";
    let consumer_source = "unit Consumer;\ninterface\nuses Provider;\nimplementation\nprocedure Consume;\nbegin\n  Log(SharedValue);\nend;\nend.\n";
    write_file(&main, main_source);
    write_file(&provider, provider_source);
    write_file(&consumer, consumer_source);
    write_file(
        &project_root.join("App.dproj"),
        "<Project><PropertyGroup><MainSource>Main.pas</MainSource><DCC_UnitSearchPath>C:\\SDK</DCC_UnitSearchPath></PropertyGroup></Project>",
    );
    write_file(
        &project_root.join(".delphi-tools.local.toml"),
        &format!(
            "[[path_mappings]]\nfrom = 'C:\\SDK'\nto = '{}'\n",
            sdk.display()
        ),
    );
    let mut server = TestServer::launch();
    server.initialize(&project_root, Value::Null);
    let declaration_id = RequestId::from("mapped-provider-declaration".to_string());
    server.send_request(
        declaration_id.clone(),
        "textDocument/declaration",
        navigation_params(&main, main_source, "SharedValue", 0),
    );
    let declaration = result_locations(server.response(&declaration_id));
    assert_eq!(declaration.len(), 1);
    assert_eq!(declaration[0]["uri"], uri(&provider).to_string());

    let references_id = RequestId::from("mapped-consumer-references".to_string());
    server.send_request(
        references_id.clone(),
        "textDocument/references",
        json!({
            "textDocument": {"uri": uri(&provider)},
            "position": position_of(provider_source, "SharedValue", 0),
            "context": {"includeDeclaration": false}
        }),
    );
    let references = result_locations(server.response(&references_id));
    let reference_uris = references
        .iter()
        .filter_map(|location| location["uri"].as_str())
        .collect::<HashSet<_>>();
    assert!(reference_uris.contains(uri(&main).as_str()));
    assert!(reference_uris.contains(uri(&consumer).as_str()));
    server.shutdown();
}

#[cfg(unix)]
#[test]
fn package_only_mapping_scans_external_consumers_and_rejects_partial_rename() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let project_root = temp.path().join("project");
    let sdk = temp.path().join("sdk");
    let main = project_root.join("Main.pas");
    let provider = sdk.join("Provider.pas");
    let consumer = sdk.join("Consumer.pas");
    let package = sdk.join("SDKPackage.dpk");
    let main_source = "unit Main;\ninterface\nuses Provider;\nimplementation\nprocedure Run;\nbegin\n  Log(SharedValue);\nend;\nend.\n";
    let provider_source =
        "unit Provider;\ninterface\nconst SharedValue = 1;\nimplementation\nend.\n";
    let consumer_source = "unit Consumer;\ninterface\nuses Provider;\nimplementation\nprocedure Consume;\nbegin\n  Log(SharedValue);\nend;\nend.\n";
    write_file(&main, main_source);
    write_file(&provider, provider_source);
    write_file(&consumer, consumer_source);
    write_file(
        &package,
        "package SDKPackage;\ncontains\n  Provider in 'Provider.pas';\nend.\n",
    );
    write_file(
        &project_root.join("App.dproj"),
        "<Project><PropertyGroup><MainSource>Main.pas</MainSource><DCC_UsePackage>SDKPackage</DCC_UsePackage></PropertyGroup></Project>",
    );
    write_file(
        &project_root.join(".delphi-tools.local.toml"),
        &format!(
            "[[path_mappings]]\nfrom = 'C:\\SDK'\nto = '{}'\n",
            sdk.display()
        ),
    );

    let mut server = TestServer::launch();
    server.initialize(&project_root, Value::Null);
    let definition_id = RequestId::from("package-only-definition".to_string());
    server.send_request(
        definition_id.clone(),
        "textDocument/definition",
        navigation_params(&main, main_source, "Provider", 0),
    );
    let definition = result_locations(server.response(&definition_id));
    assert_eq!(definition.len(), 1);
    assert_eq!(definition[0]["uri"], uri(&provider).to_string());

    let references_id = RequestId::from("package-only-references".to_string());
    server.send_request(
        references_id.clone(),
        "textDocument/references",
        json!({
            "textDocument": {"uri": uri(&provider)},
            "position": position_of(provider_source, "SharedValue", 0),
            "context": {"includeDeclaration": false}
        }),
    );
    let references = result_locations(server.response(&references_id));
    let reference_uris = references
        .iter()
        .filter_map(|location| location["uri"].as_str())
        .collect::<HashSet<_>>();
    assert!(reference_uris.contains(uri(&main).as_str()));
    assert!(reference_uris.contains(uri(&consumer).as_str()));

    let rename_id = RequestId::from("package-only-rename".to_string());
    server.send_request(
        rename_id.clone(),
        "textDocument/rename",
        json!({
            "textDocument": {"uri": uri(&provider)},
            "position": position_of(provider_source, "SharedValue", 0),
            "newName": "RenamedValue"
        }),
    );
    let response = server.response(&rename_id);
    let error = response
        .error
        .expect("package-only external consumer must reject partial rename");
    assert!(error.message.contains("outside configured workspace roots"));
    server.shutdown();
}

#[cfg(unix)]
#[test]
fn references_include_a_configured_native_source_under_an_effective_mapping() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let project_root = temp.path().join("project");
    let sdk = temp.path().join("sdk");
    let consumer = sdk.join("Consumer.pas");
    let provider = sdk.join("Provider.pas");
    let provider_source =
        "unit Provider;\ninterface\nconst SharedValue = 1;\nimplementation\nend.\n";
    let consumer_source = "unit Consumer;\ninterface\nuses Provider;\nimplementation\nprocedure Consume;\nbegin\n  Log(SharedValue);\nend;\nend.\n";
    write_file(&provider, provider_source);
    write_file(&consumer, consumer_source);
    write_file(
        &project_root.join("App.dproj"),
        "<Project><PropertyGroup><MainSource>C:\\SDK\\Provider.pas</MainSource></PropertyGroup></Project>",
    );
    write_file(
        &project_root.join(".delphi-tools.local.toml"),
        &format!(
            "[properties]\nDCC_UnitSearchPath = '{}'\n[[path_mappings]]\nfrom = 'C:\\SDK'\nto = '{}'\n",
            sdk.display(),
            sdk.display()
        ),
    );

    let mut server = TestServer::launch();
    server.initialize(&project_root, json!({"projectFile": "App.dproj"}));
    let id = RequestId::from("configured-native-mapped-references".to_string());
    server.send_request(
        id.clone(),
        "textDocument/references",
        json!({
            "textDocument": {"uri": uri(&provider)},
            "position": position_of(provider_source, "SharedValue", 0),
            "context": {"includeDeclaration": false}
        }),
    );
    let references = result_locations(server.response(&id));
    assert_eq!(references.len(), 1);
    assert_eq!(references[0]["uri"], uri(&consumer).to_string());

    server.shutdown();
}

#[cfg(unix)]
#[test]
fn references_scan_the_mapping_destination_for_configured_native_files_and_paths() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let project_root = temp.path().join("project");
    let sdk = temp.path().join("sdk");
    let subdir = sdk.join("subdir");
    let main = sdk.join("Main.pas");
    let provider = sdk.join("Provider.pas");
    let consumer = sdk.join("Consumer.pas");
    let provider_source =
        "unit Provider;\ninterface\nconst SharedValue = 1;\nimplementation\nend.\n";
    let consumer_source = "unit Consumer;\ninterface\nuses Provider;\nimplementation\nprocedure Consume;\nbegin\n  Log(SharedValue);\nend;\nend.\n";
    write_file(&main, "unit Main;\ninterface\nimplementation\nend.\n");
    write_file(&provider, provider_source);
    write_file(&consumer, consumer_source);
    write_file(
        &subdir.join("SubdirUnit.pas"),
        "unit SubdirUnit;\ninterface\nimplementation\nend.\n",
    );
    write_file(
        &project_root.join("App.dproj"),
        "<Project><PropertyGroup><MainSource>$(MainSource)</MainSource><DCC_UnitSearchPath>$(DCC_UnitSearchPath)</DCC_UnitSearchPath></PropertyGroup><ItemGroup><DCCReference Include=\"$(ProviderPath)\" /></ItemGroup></Project>",
    );
    write_file(
        &project_root.join(".delphi-tools.local.toml"),
        &format!(
            "[properties]\nMainSource = '{}'\nDCC_UnitSearchPath = '{}'\nProviderPath = '{}'\n[[path_mappings]]\nfrom = 'C:\\SDK'\nto = '{}'\n",
            main.display(),
            subdir.display(),
            provider.display(),
            sdk.display()
        ),
    );
    let mut server = TestServer::launch();
    server.initialize(&project_root, json!({"projectFile": "App.dproj"}));
    let id = RequestId::from("configured-native-destination-scan".to_string());
    server.send_request(
        id.clone(),
        "textDocument/references",
        json!({
            "textDocument": {"uri": uri(&provider)},
            "position": position_of(provider_source, "SharedValue", 0),
            "context": {"includeDeclaration": false}
        }),
    );
    let references = result_locations(server.response(&id));
    assert_eq!(references.len(), 1);
    assert_eq!(references[0]["uri"], uri(&consumer).to_string());
    server.shutdown();
}

#[cfg(unix)]
#[test]
fn summary_only_mapped_consumers_retain_their_owner_context() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let project_root = temp.path().join("project");
    let sdk = temp.path().join("sdk");
    let provider = project_root.join("Provider.pas");
    let consumer = sdk.join("Consumer.pas");
    let include = sdk.join("Safe.inc");
    let provider_source =
        "unit Provider;\ninterface\nconst SharedValue = 1;\nimplementation\nend.\n";
    let consumer_source = "unit Consumer;\ninterface\nuses Provider;\n{$I C:\\SDK\\Safe.inc}\nimplementation\nprocedure Consume;\nbegin\nend;\nend.\n";
    write_file(&provider, provider_source);
    write_file(&consumer, consumer_source);
    write_file(&include, "{$DEFINE SAFE}\n");
    write_file(
        &project_root.join("App.dproj"),
        "<Project><PropertyGroup><MainSource>Provider.pas</MainSource><DCC_UnitSearchPath>C:\\SDK</DCC_UnitSearchPath></PropertyGroup></Project>",
    );
    write_file(
        &project_root.join(".delphi-tools.local.toml"),
        &format!(
            "[[path_mappings]]\nfrom = 'C:\\SDK'\nto = '{}'\n",
            sdk.display()
        ),
    );

    let mut server = TestServer::launch();
    server.initialize(&project_root, Value::Null);
    let id = RequestId::from("summary-mapped-consumer".to_string());
    server.send_request(
        id.clone(),
        "textDocument/references",
        json!({
            "textDocument": {"uri": uri(&provider)},
            "position": position_of(provider_source, "SharedValue", 0),
            "context": {"includeDeclaration": false}
        }),
    );
    let response = server.response(&id);
    assert!(
        response.error.is_none(),
        "summary consumer failed: {response:?}"
    );
    assert!(response.result.unwrap().as_array().unwrap().is_empty());
    server.shutdown();
}

#[test]
fn rename_allows_a_legacy_relative_include_outside_the_workspace() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let project_root = temp.path().join("project");
    let main = project_root.join("Main.pas");
    let include = temp.path().join("shared/Safe.inc");
    let source = "unit Main;\ninterface\nconst BadConst = 1;\nimplementation\n{$I ../shared/Safe.inc}\nend.\n";
    write_file(&main, source);
    write_file(&include, "{$DEFINE SAFE}\n");
    write_file(
        &project_root.join("App.dproj"),
        "<Project><PropertyGroup><MainSource>Main.pas</MainSource></PropertyGroup></Project>",
    );

    let mut server = TestServer::launch();
    server.initialize(&project_root, Value::Null);
    let id = RequestId::from("legacy-relative-include".to_string());
    server.send_request(
        id.clone(),
        "textDocument/rename",
        json!({
            "textDocument": {"uri": uri(&main)},
            "position": position_of(source, "BadConst", 0),
            "newName": "GoodConst"
        }),
    );
    let response = server.response(&id);
    assert!(
        response.error.is_none(),
        "legacy include rejected: {response:?}"
    );
    assert!(response.result.is_some());
    server.shutdown();
}

#[cfg(unix)]
#[test]
fn rename_allows_nested_legacy_relative_includes_outside_the_workspace() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let project_root = temp.path().join("project");
    let shared = temp.path().join("shared");
    let main = project_root.join("Main.pas");
    let outer = shared.join("Outer.inc");
    let inner = shared.join("Inner.inc");
    let source = "unit Main;\ninterface\nconst BadConst = 1;\nimplementation\n{$I ../shared/Outer.inc}\nend.\n";
    write_file(&main, source);
    write_file(&outer, "{$I Inner.inc}\n");
    write_file(&inner, "{$DEFINE SAFE}\n");

    let mut server = TestServer::launch();
    server.initialize(&project_root, Value::Null);
    let id = RequestId::from("nested-legacy-relative-include".to_string());
    server.send_request(
        id.clone(),
        "textDocument/rename",
        json!({
            "textDocument": {"uri": uri(&main)},
            "position": position_of(source, "BadConst", 0),
            "newName": "GoodConst"
        }),
    );
    let response = server.response(&id);
    assert!(
        response.error.is_none(),
        "nested legacy include rejected: {response:?}"
    );
    assert!(response.result.is_some());
    server.shutdown();
}

#[cfg(unix)]
#[test]
fn rename_allows_a_multihop_legacy_search_include_without_overrides() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let project_root = temp.path().join("project");
    let legacy = temp.path().join("legacy");
    let shared = temp.path().join("shared");
    let main = project_root.join("Main.pas");
    let outer = project_root.join("Outer.inc");
    let via_search = legacy.join("ViaSearch.inc");
    let safe = shared.join("Safe.inc");
    let source =
        "unit Main;\ninterface\nconst BadConst = 1;\nimplementation\n{$I Outer.inc}\nend.\n";
    write_file(&main, source);
    write_file(&outer, "{$I ViaSearch.inc}\n");
    write_file(&via_search, "{$I ../shared/Safe.inc}\n");
    write_file(&safe, "{$DEFINE SAFE}\n");
    write_file(
        &project_root.join("App.dproj"),
        "<Project><PropertyGroup><MainSource>Main.pas</MainSource><DCC_IncludePath>../legacy</DCC_IncludePath></PropertyGroup></Project>",
    );

    let mut server = TestServer::launch();
    server.initialize(&project_root, Value::Null);
    let id = RequestId::from("multihop-legacy-search-include".to_string());
    server.send_request(
        id.clone(),
        "textDocument/rename",
        json!({
            "textDocument": {"uri": uri(&main)},
            "position": position_of(source, "BadConst", 0),
            "newName": "GoodConst"
        }),
    );
    let response = server.response(&id);
    assert!(
        response.error.is_none(),
        "multihop legacy search include rejected: {response:?}"
    );
    assert!(response.result.is_some());
    server.shutdown();
}

#[cfg(unix)]
#[test]
fn rename_allows_a_top_level_legacy_search_include_without_overrides() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let project_root = temp.path().join("project");
    let legacy = temp.path().join("legacy");
    let shared = temp.path().join("shared");
    let main = project_root.join("Main.pas");
    let via_search = legacy.join("ViaSearch.inc");
    let safe = shared.join("Safe.inc");
    let source =
        "unit Main;\ninterface\nconst BadConst = 1;\nimplementation\n{$I ViaSearch.inc}\nend.\n";
    write_file(&main, source);
    write_file(&via_search, "{$I ../shared/Safe.inc}\n");
    write_file(&safe, "{$DEFINE SAFE}\n");
    write_file(
        &project_root.join("App.dproj"),
        "<Project><PropertyGroup><MainSource>Main.pas</MainSource><DCC_IncludePath>../legacy</DCC_IncludePath></PropertyGroup></Project>",
    );

    let mut server = TestServer::launch();
    server.initialize(&project_root, Value::Null);
    let id = RequestId::from("top-level-legacy-search-include".to_string());
    server.send_request(
        id.clone(),
        "textDocument/rename",
        json!({
            "textDocument": {"uri": uri(&main)},
            "position": position_of(source, "BadConst", 0),
            "newName": "GoodConst"
        }),
    );
    let response = server.response(&id);
    assert!(
        response.error.is_none(),
        "top-level legacy search include rejected: {response:?}"
    );
    assert!(response.result.is_some());
    server.shutdown();
}

#[cfg(unix)]
#[test]
fn rename_allows_a_literal_native_absolute_include_outside_the_workspace() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let project_root = temp.path().join("project");
    let shared = temp.path().join("shared");
    let main = project_root.join("Main.pas");
    let include = shared.join("Safe.inc");
    let source = format!(
        "unit Main;\ninterface\nconst BadConst = 1;\nimplementation\n{{$I {}}}\nend.\n",
        include.display()
    );
    write_file(&main, &source);
    write_file(&include, "{$DEFINE SAFE}\n");

    let mut server = TestServer::launch();
    server.initialize(&project_root, Value::Null);
    let id = RequestId::from("literal-native-absolute-include".to_string());
    server.send_request(
        id.clone(),
        "textDocument/rename",
        json!({
            "textDocument": {"uri": uri(&main)},
            "position": position_of(&source, "BadConst", 0),
            "newName": "GoodConst"
        }),
    );
    let response = server.response(&id);
    assert!(
        response.error.is_none(),
        "native absolute include rejected: {response:?}"
    );
    assert!(response.result.is_some());
    server.shutdown();
}

#[cfg(unix)]
#[test]
fn rename_rejects_a_configured_include_path_outside_allowed_roots() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let project_root = temp.path().join("project");
    let external = temp.path().join("external");
    let main = project_root.join("Main.pas");
    let include = external.join("Safe.inc");
    let source =
        "unit Main;\ninterface\nconst BadConst = 1;\nimplementation\n{$I Safe.inc}\nend.\n";
    write_file(&main, source);
    write_file(&include, "{$DEFINE SAFE}\n");
    write_file(
        &project_root.join("App.dproj"),
        "<Project><PropertyGroup><MainSource>Main.pas</MainSource></PropertyGroup></Project>",
    );
    write_file(
        &project_root.join(".delphi-tools.local.toml"),
        &format!("[properties]\nDCC_IncludePath = '{}'\n", external.display()),
    );

    let mut server = TestServer::launch();
    server.initialize(&project_root, Value::Null);
    let id = RequestId::from("configured-include-outside-roots".to_string());
    server.send_request(
        id.clone(),
        "textDocument/rename",
        json!({
            "textDocument": {"uri": uri(&main)},
            "position": position_of(source, "BadConst", 0),
            "newName": "GoodConst"
        }),
    );
    let response = server.response(&id);
    let error = response
        .error
        .expect("configured external include must be rejected");
    assert!(
        error
            .message
            .contains("outside the owning project's readable roots")
    );
    assert!(response.result.is_none());
    server.shutdown();
}

#[cfg(unix)]
#[test]
fn rename_rejects_a_nested_include_selected_from_a_configured_path_after_a_legacy_include() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let project_root = temp.path().join("project");
    let configured_include_root = temp.path().join("configured-includes");
    let main = project_root.join("Main.pas");
    let outer = project_root.join("Outer.inc");
    let safe = configured_include_root.join("Safe.inc");
    let source =
        "unit Main;\ninterface\nconst BadConst = 1;\nimplementation\n{$I Outer.inc}\nend.\n";
    write_file(&main, source);
    write_file(&outer, "{$I Safe.inc}\n");
    write_file(&safe, "{$DEFINE SAFE}\n");
    write_file(
        &project_root.join("App.dproj"),
        "<Project><PropertyGroup><MainSource>Main.pas</MainSource></PropertyGroup></Project>",
    );
    write_file(
        &project_root.join(".delphi-tools.local.toml"),
        &format!(
            "[properties]\nDCC_IncludePath = '{}'\n",
            configured_include_root.display()
        ),
    );

    let mut server = TestServer::launch();
    server.initialize(&project_root, Value::Null);
    let id = RequestId::from("nested-configured-include".to_string());
    server.send_request(
        id.clone(),
        "textDocument/rename",
        json!({
            "textDocument": {"uri": uri(&main)},
            "position": position_of(source, "BadConst", 0),
            "newName": "GoodConst"
        }),
    );
    let response = server.response(&id);
    let error = response
        .error
        .expect("nested configured include must be rejected");
    assert!(
        error
            .message
            .contains("outside the owning project's readable roots")
    );
    assert!(response.result.is_none());
    server.shutdown();
}

#[cfg(unix)]
#[test]
fn references_use_a_mapped_overlay_after_its_disk_source_is_removed() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let project_root = temp.path().join("project");
    let sdk = temp.path().join("sdk");
    let provider = project_root.join("Provider.pas");
    let consumer = sdk.join("Consumer.pas");
    let provider_source =
        "unit Provider;\ninterface\nconst SharedValue = 1;\nimplementation\nend.\n";
    let disk_consumer = "unit Consumer;\ninterface\nuses Provider;\nimplementation\nend.\n";
    let overlay_consumer = "unit Consumer;\ninterface\nuses Provider;\nimplementation\nprocedure Consume;\nbegin\n  Log(SharedValue);\nend;\nend.\n";
    write_file(&provider, provider_source);
    write_file(&consumer, disk_consumer);
    write_file(
        &project_root.join("App.dproj"),
        "<Project><PropertyGroup><MainSource>Provider.pas</MainSource><DCC_UnitSearchPath>C:\\SDK</DCC_UnitSearchPath></PropertyGroup></Project>",
    );
    write_file(
        &project_root.join(".delphi-tools.local.toml"),
        &format!(
            "[[path_mappings]]\nfrom = 'C:\\SDK'\nto = '{}'\n",
            sdk.display()
        ),
    );

    let mut server = TestServer::launch();
    server.initialize(&project_root, json!({"projectFile": "App.dproj"}));
    server.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri(&consumer),
                "languageId": "pascal",
                "version": 7,
                "text": overlay_consumer
            }
        }),
    );
    fs::remove_file(&consumer).expect("remove disk source behind overlay");

    let id = RequestId::from("mapped-overlay-references".to_string());
    server.send_request(
        id.clone(),
        "textDocument/references",
        json!({
            "textDocument": {"uri": uri(&provider)},
            "position": position_of(provider_source, "SharedValue", 0),
            "context": {"includeDeclaration": false}
        }),
    );
    let references = result_locations(server.response(&id));
    assert_eq!(references.len(), 1);
    assert_eq!(references[0]["uri"], uri(&consumer).to_string());

    server.send_notification(
        "textDocument/didChange",
        json!({
            "textDocument": {"uri": uri(&consumer), "version": 8},
            "contentChanges": [{"text": "unit Consumer;\ninterface\nuses Provider;\nimplementation\nend.\n"}]
        }),
    );
    let changed_id = RequestId::from("mapped-overlay-references-after-change".to_string());
    server.send_request(
        changed_id.clone(),
        "textDocument/references",
        json!({
            "textDocument": {"uri": uri(&provider)},
            "position": position_of(provider_source, "SharedValue", 0),
            "context": {"includeDeclaration": false}
        }),
    );
    assert!(result_locations(server.response(&changed_id)).is_empty());
    server.shutdown();
}

#[cfg(unix)]
#[test]
fn references_keep_same_named_mapped_overlays_in_their_owner_context() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let workspace_root = temp.path().join("workspace");
    let project_a = temp.path().join("project-a");
    let project_b = temp.path().join("project-b");
    let sdk_a = project_a.join("sdk");
    let sdk_b = project_b.join("sdk");
    let provider_a = sdk_a.join("Provider.pas");
    let provider_b = sdk_b.join("Provider.pas");
    let consumer_a = sdk_a.join("Consumer.pas");
    let consumer_b = sdk_b.join("Consumer.pas");
    let provider_source =
        "unit Provider;\ninterface\nconst SharedValue = 1;\nimplementation\nend.\n";
    let safe_consumer_source = "unit Consumer;\ninterface\nuses Provider;\nimplementation\nprocedure Consume;\nbegin\n  Log(SharedValue);\nend;\nend.\n";
    let poisoned_consumer_source = "unit Consumer;\ninterface\nuses Provider;\n{$I Missing.inc}\nimplementation\nprocedure Consume;\nbegin\n  Log(SharedValue);\nend;\nend.\n";
    fs::create_dir_all(&workspace_root).expect("workspace root");
    for (project, sdk) in [(&project_a, &sdk_a), (&project_b, &sdk_b)] {
        write_file(
            &project.join("App.dproj"),
            "<Project><PropertyGroup><MainSource>C:\\SDK\\Provider.pas</MainSource><DCC_UnitSearchPath>C:\\SDK</DCC_UnitSearchPath></PropertyGroup></Project>",
        );
        write_file(
            &project.join(".delphi-tools.local.toml"),
            &format!(
                "[[path_mappings]]\nfrom = 'C:\\SDK'\nto = '{}'\n",
                sdk.display()
            ),
        );
    }
    write_file(&provider_a, provider_source);
    write_file(&provider_b, provider_source);

    let mut server = TestServer::launch();
    server.initialize(&workspace_root, Value::Null);
    for (consumer, source) in [
        (&consumer_a, safe_consumer_source),
        (&consumer_b, poisoned_consumer_source),
    ] {
        server.send_notification(
            "textDocument/didOpen",
            json!({
                "textDocument": {
                    "uri": uri(consumer),
                    "languageId": "pascal",
                    "version": 1,
                    "text": source
                }
            }),
        );
    }

    let owner_a_id = RequestId::from("owner-a-mapped-overlay".to_string());
    server.send_request(
        owner_a_id.clone(),
        "textDocument/references",
        json!({
            "textDocument": {"uri": uri(&provider_a)},
            "position": position_of(provider_source, "SharedValue", 0),
            "context": {"includeDeclaration": false}
        }),
    );
    let owner_a_references = result_locations(server.response(&owner_a_id));
    assert_eq!(owner_a_references.len(), 1);
    assert_eq!(owner_a_references[0]["uri"], uri(&consumer_a).to_string());

    for (consumer, source) in [
        (&consumer_a, poisoned_consumer_source),
        (&consumer_b, safe_consumer_source),
    ] {
        server.send_notification(
            "textDocument/didChange",
            json!({
                "textDocument": {"uri": uri(consumer), "version": 2},
                "contentChanges": [{"text": source}]
            }),
        );
    }
    let owner_b_id = RequestId::from("owner-b-mapped-overlay".to_string());
    server.send_request(
        owner_b_id.clone(),
        "textDocument/references",
        json!({
            "textDocument": {"uri": uri(&provider_b)},
            "position": position_of(provider_source, "SharedValue", 0),
            "context": {"includeDeclaration": false}
        }),
    );
    let owner_b_references = result_locations(server.response(&owner_b_id));
    assert_eq!(owner_b_references.len(), 1);
    assert_eq!(owner_b_references[0]["uri"], uri(&consumer_b).to_string());
    server.shutdown();
}

#[cfg(unix)]
#[test]
fn captured_override_edits_do_not_stale_mapped_reference_snapshots() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let project_root = temp.path().join("project");
    let sdk = temp.path().join("sdk");
    let changed_sdk = temp.path().join("changed-sdk");
    let provider = project_root.join("Provider.pas");
    let consumer = sdk.join("Consumer.pas");
    let provider_source =
        "unit Provider;\ninterface\nconst SharedValue = 1;\nimplementation\nend.\n";
    let consumer_source = "unit Consumer;\ninterface\nuses Provider;\nimplementation\nprocedure Consume;\nbegin\n  Log(SharedValue);\nend;\nend.\n";
    write_file(&provider, provider_source);
    write_file(&consumer, consumer_source);
    write_file(
        &project_root.join("App.dproj"),
        "<Project><PropertyGroup><MainSource>Provider.pas</MainSource><DCC_UnitSearchPath>C:\\SDK</DCC_UnitSearchPath></PropertyGroup></Project>",
    );
    write_file(
        &project_root.join(".delphi-tools.local.toml"),
        &format!(
            "[[path_mappings]]\nfrom = 'C:\\SDK'\nto = '{}'\n",
            sdk.display()
        ),
    );

    let mut server = TestServer::launch();
    server.initialize(&project_root, Value::Null);
    let request = |server: &mut TestServer, id: &str| {
        let id = RequestId::from(id.to_string());
        server.send_request(
            id.clone(),
            "textDocument/references",
            json!({
                "textDocument": {"uri": uri(&provider)},
                "position": position_of(provider_source, "SharedValue", 0),
                "context": {"includeDeclaration": false}
            }),
        );
        result_locations(server.response(&id))
    };

    let first = request(&mut server, "captured-overrides-before-edit");
    assert_eq!(first.len(), 1);
    assert_eq!(first[0]["uri"], uri(&consumer).to_string());
    write_file(
        &project_root.join(".delphi-tools.local.toml"),
        &format!(
            "[properties]\nDCC_UnitSearchPath = '{}'\n",
            changed_sdk.display()
        ),
    );

    for id in [
        "captured-overrides-after-edit",
        "captured-overrides-after-edit-again",
    ] {
        let references = request(&mut server, id);
        assert_eq!(references.len(), 1);
        assert_eq!(references[0]["uri"], uri(&consumer).to_string());
    }
    server.shutdown();
}

#[cfg(unix)]
#[test]
fn rename_rejects_edits_to_a_mapped_external_consumer() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let project_root = temp.path().join("project");
    let sdk = temp.path().join("sdk");
    let provider = project_root.join("Provider.pas");
    let consumer = sdk.join("Consumer.pas");
    let provider_source = "unit Provider;\ninterface\nconst BadConst = 1;\nimplementation\nend.\n";
    let consumer_source = "unit Consumer;\ninterface\nuses Provider;\nimplementation\nprocedure Consume;\nbegin\n  Log(BadConst);\nend;\nend.\n";
    write_file(&provider, provider_source);
    write_file(&consumer, consumer_source);
    write_file(
        &project_root.join("App.dproj"),
        "<Project><PropertyGroup><MainSource>Provider.pas</MainSource><DCC_UnitSearchPath>C:\\SDK</DCC_UnitSearchPath></PropertyGroup></Project>",
    );
    write_file(
        &project_root.join(".delphi-tools.local.toml"),
        &format!(
            "[[path_mappings]]\nfrom = 'C:\\SDK'\nto = '{}'\n",
            sdk.display()
        ),
    );

    let mut server = TestServer::launch();
    server.initialize(&project_root, Value::Null);
    let rename_id = RequestId::from("mapped-consumer-rename".to_string());
    server.send_request(
        rename_id.clone(),
        "textDocument/rename",
        json!({
            "textDocument": {"uri": uri(&provider)},
            "position": position_of(provider_source, "BadConst", 0),
            "newName": "GoodConst"
        }),
    );
    let response = server.response(&rename_id);
    let error = response
        .error
        .expect("mapped external edit must be rejected");
    assert!(error.message.contains("outside configured workspace roots"));
    assert!(response.result.is_none());
    assert_eq!(fs::read_to_string(&provider).unwrap(), provider_source);
    assert_eq!(fs::read_to_string(&consumer).unwrap(), consumer_source);
    server.shutdown();
}

#[cfg(unix)]
#[test]
fn references_reject_a_missing_nested_include_in_a_mapped_consumer() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let project_root = temp.path().join("project");
    let sdk = temp.path().join("sdk");
    let provider = project_root.join("Provider.pas");
    let consumer = sdk.join("Consumer.pas");
    let outer_include = sdk.join("Nested/Outer.inc");
    let provider_source =
        "unit Provider;\ninterface\nconst SharedValue = 1;\nimplementation\nend.\n";
    let consumer_source = "unit Consumer;\ninterface\nuses Provider;\n{$I C:\\SDK\\Nested\\Outer.inc}\nimplementation\nprocedure Consume;\nbegin\n  Log(SharedValue);\nend;\nend.\n";
    write_file(&provider, provider_source);
    write_file(&consumer, consumer_source);
    write_file(&outer_include, "{$I C:\\SDK\\Nested\\Missing.inc}\n");
    write_file(
        &project_root.join("App.dproj"),
        "<Project><PropertyGroup><MainSource>Provider.pas</MainSource><DCC_UnitSearchPath>C:\\SDK</DCC_UnitSearchPath></PropertyGroup></Project>",
    );
    write_file(
        &project_root.join(".delphi-tools.local.toml"),
        &format!(
            "[[path_mappings]]\nfrom = 'C:\\SDK'\nto = '{}'\n",
            sdk.display()
        ),
    );

    let mut server = TestServer::launch();
    server.initialize(&project_root, Value::Null);
    let references_id = RequestId::from("mapped-missing-nested-include".to_string());
    server.send_request(
        references_id.clone(),
        "textDocument/references",
        json!({
            "textDocument": {"uri": uri(&provider)},
            "position": position_of(provider_source, "SharedValue", 0),
            "context": {"includeDeclaration": false}
        }),
    );
    let response = server.response(&references_id);
    let error = response
        .error
        .expect("missing mapped nested include must reject references");
    assert!(error.message.contains("workspace scan incomplete"));
    assert!(response.result.is_none());
    server.shutdown();
}

#[cfg(target_os = "linux")]
#[test]
fn mapped_include_cannot_grant_legacy_authority_to_an_outside_nested_include() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let project_root = temp.path().join("project");
    let sdk = temp.path().join("sdk");
    let provider = project_root.join("Provider.pas");
    let outer_include = sdk.join("Nested/Outer.inc");
    let outside_include = temp.path().join("outside.inc");
    let provider_source = "unit Provider;\ninterface\nconst SharedValue = 1;\nimplementation\n{$I C:\\SDK\\Nested\\Outer.inc}\nend.\n";
    write_file(&provider, provider_source);
    write_file(&outer_include, "{$I ../../outside.inc}\n");
    write_file(&outside_include, "{$DEFINE SAFE}\n");
    write_file(
        &project_root.join("App.dproj"),
        "<Project><PropertyGroup><MainSource>Provider.pas</MainSource><DCC_UnitSearchPath>C:\\SDK</DCC_UnitSearchPath></PropertyGroup></Project>",
    );
    write_file(
        &project_root.join(".delphi-tools.local.toml"),
        &format!(
            "[[path_mappings]]\nfrom = 'C:\\SDK'\nto = '{}'\n",
            sdk.display()
        ),
    );

    let opened = observed_open(&outside_include, || {
        let mut server = TestServer::launch();
        server.initialize(&project_root, Value::Null);
        let references_id = RequestId::from("mapped-nested-escape".to_string());
        server.send_request(
            references_id.clone(),
            "textDocument/references",
            json!({
                "textDocument": {"uri": uri(&provider)},
                "position": position_of(provider_source, "SharedValue", 0),
                "context": {"includeDeclaration": false}
            }),
        );
        let response = server.response(&references_id);
        let error = response
            .error
            .expect("outside nested include must reject references");
        assert!(
            error
                .message
                .contains("outside the owning project's readable roots")
        );
        server.shutdown();
    });

    assert!(!opened, "mapped nested include escaped its authorized root");
}

#[cfg(target_os = "linux")]
#[test]
fn rename_fingerprint_does_not_reclassify_a_denied_recursive_candidate() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path().join("workspace");
    let sdk = root.join("sdk");
    let outside = root.join("outside");
    let target = root.join("Shared.pas");
    let denied = outside.join("Denied.pas");
    let target_source = "unit Shared;\ninterface\nconst BadConst = 1;\nimplementation\nend.\n";

    write_file(&target, target_source);
    write_file(&denied, "unit Denied; interface implementation end.\n");
    write_file(
        &sdk.join("App.dpr"),
        "program App; uses Denied in '../outside/Denied.pas'; begin end.\n",
    );
    write_file(&root.join("B.dpr"), "program B; begin end.\n");
    write_file(
        &root.join("A.dproj"),
        "<Project><PropertyGroup><MainSource>C:\\SDK\\App.dpr</MainSource></PropertyGroup></Project>",
    );
    write_file(
        &root.join("B.dproj"),
        "<Project><PropertyGroup><MainSource>B.dpr</MainSource></PropertyGroup></Project>",
    );
    write_file(
        &root.join(".delphi-tools.local.toml"),
        &format!(
            "[[path_mappings]]\nfrom = 'C:\\SDK'\nto = '{}'\n",
            sdk.display()
        ),
    );

    let opened = observed_open(&denied, || {
        let mut workspace = test_workspace(vec![root.clone()], WorkspaceOptions::default());
        let result = workspace.rename_edits(
            &uri(&target),
            position_of(target_source, "BadConst", 0),
            "GoodConst",
            false,
        );
        assert!(
            result.is_err(),
            "an incomplete candidate context must not produce a rename"
        );
    });

    assert!(
        !opened,
        "fingerprinting must not grant the final context's workspace root to a denied recursive candidate"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn exists_only_excluded_metadata_is_never_fingerprinted() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path().join("workspace");
    let private = root.join("vendor/private/settings.optset");
    let target = root.join("Provider.pas");
    let target_source = "unit Provider;\ninterface\nconst BadConst = 1;\nimplementation\nend.\n";

    write_file(
        &private,
        "<Project><PropertyGroup><DCC_Define>NOPE</DCC_Define></PropertyGroup></Project>",
    );
    write_file(&target, target_source);
    write_file(&root.join("App.dpr"), "program App; begin end.\n");
    write_file(
        &root.join("App.dproj"),
        "<Project><PropertyGroup><MainSource>App.dpr</MainSource></PropertyGroup><PropertyGroup Condition=\"Exists('vendor/private/settings.optset')\"><DCC_Define>PRIVATE_SETTINGS</DCC_Define></PropertyGroup></Project>",
    );

    let opened = observed_open(&private, || {
        let mut workspace = test_workspace(
            vec![root.clone()],
            WorkspaceOptions {
                source_paths: vec!["vendor".to_string()],
                exclude: vec!["vendor/private".to_string()],
                ..WorkspaceOptions::default()
            },
        );
        let result = workspace.rename_edits(
            &uri(&target),
            position_of(target_source, "BadConst", 0),
            "GoodConst",
            false,
        );
        assert!(
            result.is_ok(),
            "a complete project request must succeed: {result:?}"
        );
    });

    assert!(
        !opened,
        "Exists-only excluded metadata must remain stat-only"
    );
}

#[cfg(unix)]
#[test]
fn references_do_not_follow_mapped_symlink_escapes_or_sibling_prefixes() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let project_root = temp.path().join("project");
    let sdk = temp.path().join("sdk");
    let sibling = temp.path().join("sdk-old");
    let outside = temp.path().join("outside");
    let provider = project_root.join("Provider.pas");
    let provider_source =
        "unit Provider;\ninterface\nconst SharedValue = 1;\nimplementation\nend.\n";
    let consumer_source = "unit Consumer;\ninterface\nuses Provider;\nimplementation\nprocedure Consume;\nbegin\n  Log(SharedValue);\nend;\nend.\n";
    write_file(&provider, provider_source);
    write_file(&outside.join("Consumer.pas"), consumer_source);
    write_file(&sibling.join("Consumer.pas"), consumer_source);
    fs::create_dir_all(&sdk).expect("mapped SDK directory");
    symlink(&outside, sdk.join("escape")).expect("mapped symlink escape");
    write_file(
        &project_root.join("App.dproj"),
        "<Project><PropertyGroup><MainSource>Provider.pas</MainSource><DCC_UnitSearchPath>C:\\SDK</DCC_UnitSearchPath></PropertyGroup></Project>",
    );
    write_file(
        &project_root.join(".delphi-tools.local.toml"),
        &format!(
            "[[path_mappings]]\nfrom = 'C:\\SDK'\nto = '{}'\n",
            sdk.display()
        ),
    );

    let mut server = TestServer::launch();
    server.initialize(&project_root, Value::Null);
    let references_id = RequestId::from("mapped-containment".to_string());
    server.send_request(
        references_id.clone(),
        "textDocument/references",
        json!({
            "textDocument": {"uri": uri(&provider)},
            "position": position_of(provider_source, "SharedValue", 0),
            "context": {"includeDeclaration": false}
        }),
    );
    let response = server.response(&references_id);
    assert!(
        response.error.is_none(),
        "unexpected response error: {response:?}"
    );
    assert!(
        response
            .result
            .expect("reference result")
            .as_array()
            .expect("reference array")
            .is_empty()
    );
    server.shutdown();
}

#[cfg(target_os = "linux")]
#[test]
fn legacy_sibling_grant_cannot_bypass_mapped_explicit_symlink_safety() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let project_root = temp.path().join("project");
    let outside = temp.path().join("outside");
    let main = project_root.join("Main.pas");
    let mapped_provider = project_root.join("Provider.pas");
    let outside_provider = outside.join("Provider.pas");
    let provider_source =
        "unit Provider;\ninterface\nconst SharedValue = 1;\nimplementation\nend.\n";
    let main_source = "unit Main;\ninterface\nuses Provider;\nimplementation\nprocedure Consume;\nbegin\n  Log(SharedValue);\nend;\nend.\n";
    write_file(&outside_provider, provider_source);
    write_file(&main, main_source);
    fs::create_dir_all(&project_root).expect("project directory");
    symlink(&outside_provider, &mapped_provider).expect("mapped provider escape");
    write_file(
        &project_root.join("App.dproj"),
        "<Project><PropertyGroup><MainSource>Main.pas</MainSource></PropertyGroup><ItemGroup><DCCReference Include=\"C:\\SDK\\Provider.pas\" /></ItemGroup></Project>",
    );
    write_file(
        &project_root.join(".delphi-tools.local.toml"),
        &format!(
            "[[path_mappings]]\nfrom = 'C:\\SDK'\nto = '{}'\n",
            project_root.display()
        ),
    );

    let opened = observed_open(&outside_provider, || {
        let mut server = TestServer::launch();
        server.initialize(&project_root, Value::Null);
        let definition_id = RequestId::from("mapped-explicit-symlink-safety".to_string());
        server.send_request(
            definition_id.clone(),
            "textDocument/definition",
            json!({
                "textDocument": {"uri": uri(&main)},
                "position": position_of(main_source, "SharedValue", 0)
            }),
        );
        let response = server.response(&definition_id);
        assert!(
            response.error.is_none(),
            "mapped symlink lookup returned an unexpected error: {response:?}"
        );
        assert_eq!(
            response.result,
            Some(Value::Array(Vec::new())),
            "mapped symlink target was indexed through the legacy sibling grant"
        );
        server.shutdown();
    });

    assert!(!opened, "mapped explicit symlink target was opened");
}

#[cfg(unix)]
#[test]
fn references_reject_exhausted_mapped_source_budgets() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let project_root = temp.path().join("project");
    let sdk = temp.path().join("sdk");
    let provider = project_root.join("Provider.pas");
    let consumer = sdk.join("Consumer.pas");
    let provider_source =
        "unit Provider;\ninterface\nconst SharedValue = 1;\nimplementation\nend.\n";
    let consumer_source = "unit Consumer;\ninterface\nuses Provider;\nimplementation\nprocedure Consume;\nbegin\n  Log(SharedValue);\nend;\nend.\n";
    write_file(&provider, provider_source);
    write_file(&consumer, consumer_source);
    write_file(
        &project_root.join("App.dproj"),
        "<Project><PropertyGroup><MainSource>Provider.pas</MainSource><DCC_UnitSearchPath>C:\\SDK</DCC_UnitSearchPath></PropertyGroup></Project>",
    );
    write_file(
        &project_root.join(".delphi-tools.local.toml"),
        &format!(
            "[[path_mappings]]\nfrom = 'C:\\SDK'\nto = '{}'\n",
            sdk.display()
        ),
    );

    let mut server = TestServer::launch();
    server.initialize(&project_root, json!({"maxFiles": 1}));
    let references_id = RequestId::from("mapped-budget".to_string());
    server.send_request(
        references_id.clone(),
        "textDocument/references",
        json!({
            "textDocument": {"uri": uri(&provider)},
            "position": position_of(provider_source, "SharedValue", 0),
            "context": {"includeDeclaration": false}
        }),
    );
    let response = server.response(&references_id);
    let error = response
        .error
        .expect("mapped source budget exhaustion must reject references");
    assert!(error.message.contains("incomplete"));
    assert!(response.result.is_none());
    server.shutdown();
}

#[cfg(unix)]
#[test]
fn references_do_not_use_a_mapping_from_an_unrelated_project_context() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path().join("workspace");
    let project_a = root.join("A");
    let project_b = root.join("B");
    let sdk = temp.path().join("sdk");
    let provider = project_a.join("Provider.pas");
    let main_a = project_a.join("Main.pas");
    let consumer = sdk.join("Consumer.pas");
    let provider_source =
        "unit Provider;\ninterface\nconst SharedValue = 1;\nimplementation\nend.\n";
    let main_source = "unit Main;\ninterface\nuses Provider;\nimplementation\nprocedure Run;\nbegin\n  Log(SharedValue);\nend;\nend.\n";
    let consumer_source = "unit Consumer;\ninterface\nuses Provider;\nimplementation\nprocedure Consume;\nbegin\n  Log(SharedValue);\nend;\nend.\n";
    write_file(&provider, provider_source);
    write_file(&main_a, main_source);
    write_file(&consumer, consumer_source);
    write_file(
        &project_a.join("App.dproj"),
        "<Project><PropertyGroup><MainSource>Main.pas</MainSource></PropertyGroup></Project>",
    );
    write_file(
        &project_b.join("Main.pas"),
        "unit Main; interface implementation end.\n",
    );
    write_file(
        &project_b.join("App.dproj"),
        "<Project><PropertyGroup><MainSource>Main.pas</MainSource><DCC_UnitSearchPath>C:\\SDK</DCC_UnitSearchPath></PropertyGroup><ItemGroup><DCCReference Include=\"..\\A\\Provider.pas\" /></ItemGroup></Project>",
    );
    write_file(
        &project_b.join(".delphi-tools.local.toml"),
        &format!(
            "[[path_mappings]]\nfrom = 'C:\\SDK'\nto = '{}'\n",
            sdk.display()
        ),
    );

    let mut server = TestServer::launch();
    server.initialize(&root, Value::Null);
    let references_id = RequestId::from("unrelated-project-mapping".to_string());
    server.send_request(
        references_id.clone(),
        "textDocument/references",
        json!({
            "textDocument": {"uri": uri(&provider)},
            "position": position_of(provider_source, "SharedValue", 0),
            "context": {"includeDeclaration": false}
        }),
    );
    let references = result_locations(server.response(&references_id));
    assert!(!references.is_empty());
    assert!(
        references
            .iter()
            .all(|location| location["uri"] == uri(&main_a).to_string()),
        "project B's mapping must not add its external consumer: {references:?}"
    );
    server.shutdown();
}

#[cfg(unix)]
#[test]
fn workspace_symbols_enumerate_mapped_sources_from_project_metadata() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let project_root = temp.path().join("project");
    let sdk = temp.path().join("sdk");
    let mapped_source = sdk.join("Mapped.pas");
    write_file(
        &mapped_source,
        "unit Mapped;\ninterface\nprocedure MappedThing;\nimplementation\nend.\n",
    );
    write_file(
        &project_root.join("App.dproj"),
        "<Project><PropertyGroup><MainSource>C:\\SDK\\Mapped.pas</MainSource></PropertyGroup></Project>",
    );
    write_file(
        &project_root.join(".delphi-tools.local.toml"),
        &format!(
            "[[path_mappings]]\nfrom = 'C:\\SDK'\nto = '{}'\n",
            sdk.display()
        ),
    );

    let mut server = TestServer::launch();
    server.initialize(&project_root, Value::Null);
    let symbols_id = RequestId::from("mapped-workspace-symbols".to_string());
    server.send_request(
        symbols_id.clone(),
        "workspace/symbol",
        json!({"query": "MappedThing"}),
    );
    let response = server.response(&symbols_id);
    assert!(
        response.error.is_none(),
        "mapped workspace symbols failed: {response:?}"
    );
    let result = response.result.expect("mapped workspace symbols result");
    let symbols = result.as_array().expect("mapped workspace symbols array");
    assert_eq!(symbols.len(), 1);
    assert_eq!(
        symbols[0]["location"]["uri"],
        uri(&mapped_source).to_string()
    );
    server.shutdown();
}

#[test]
fn workspace_symbols_reject_incomplete_override_context_without_mapped_roots() {
    let temp = tempfile::tempdir().unwrap();
    let project_root = temp.path().join("project");
    let source = project_root.join("Main.pas");
    write_file(
        &source,
        "unit Main;\ninterface\nprocedure IncompleteThing;\nimplementation\nend.\n",
    );
    write_file(
        &project_root.join("App.dproj"),
        "<Project><PropertyGroup><MainSource>Main.pas</MainSource></PropertyGroup></Project>",
    );
    write_file(
        &project_root.join(".delphi-tools.local.toml"),
        "[properties\n",
    );

    let mut server = TestServer::launch();
    server.initialize(&project_root, Value::Null);
    let id = RequestId::from("incomplete-workspace-symbols".to_string());
    server.send_request(
        id.clone(),
        "workspace/symbol",
        json!({"query": "IncompleteThing"}),
    );
    let response = server.response(&id);
    let error = response
        .error
        .expect("workspace symbols must fail on malformed override configuration");
    assert!(error.message.contains("incomplete"), "{error:?}");
    server.shutdown();
}

#[cfg(unix)]
#[test]
fn workspace_symbols_retain_same_directory_mapped_project_contexts() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path().join("workspace");
    let sdk = temp.path().join("sdk");
    let ordinary_main = root.join("AMain.pas");
    let mapped_source = sdk.join("MappedOnly.pas");

    write_file(
        &ordinary_main,
        "unit AMain;\ninterface\nprocedure OrdinaryThing;\nimplementation\nend.\n",
    );
    write_file(
        &mapped_source,
        "unit MappedOnly;\ninterface\nprocedure MappedOnlyThing;\nimplementation\nend.\n",
    );
    write_file(
        &root.join("0B.dproj"),
        "<Project><PropertyGroup><MainSource>AMain.pas</MainSource><DCC_UnitSearchPath>C:\\SDK</DCC_UnitSearchPath></PropertyGroup></Project>",
    );
    write_file(
        &root.join("1A.dproj"),
        "<Project><PropertyGroup><MainSource>AMain.pas</MainSource></PropertyGroup></Project>",
    );
    write_file(
        &root.join(".delphi-tools.local.toml"),
        &format!(
            "[[path_mappings]]\nfrom = 'C:\\SDK'\nto = '{}'\n",
            sdk.display()
        ),
    );

    let mut server = TestServer::launch();
    server.initialize(&root, Value::Null);
    let id = RequestId::from("same-directory-mapped-contexts".to_string());
    server.send_request(
        id.clone(),
        "workspace/symbol",
        json!({"query": "MappedOnlyThing"}),
    );
    let response = server.response(&id);
    assert!(
        response.error.is_none(),
        "mapped context was lost: {response:?}"
    );
    let symbols = response
        .result
        .expect("workspace symbols result")
        .as_array()
        .expect("workspace symbols array")
        .clone();
    assert_eq!(symbols.len(), 1);
    assert_eq!(
        symbols[0]["location"]["uri"],
        uri(&mapped_source).to_string()
    );
    server.shutdown();
}

#[test]
fn class_field_queries_keep_declarations_accessors_and_renames_bound() {
    let first = "  TFirst = class\n    FValue: Integer;\n    property Value: Integer read FValue;\n  end;\n";
    let second = "  TSecond = class\n    FValue: Integer;\n  end;\n";

    for (case_name, first_before_second) in [
        ("first-before-second", true),
        ("second-before-first", false),
    ] {
        let temp = tempfile::tempdir().expect("temporary workspace");
        let source_path = temp.path().join("ReviewCache.pas");
        let source = if first_before_second {
            format!("unit ReviewCache;\ninterface\ntype\n{first}{second}implementation\nend.\n")
        } else {
            format!("unit ReviewCache;\ninterface\ntype\n{second}{first}implementation\nend.\n")
        };
        write_file(&source_path, &source);
        let disk_source = fs::read(&source_path).expect("read fixture bytes");
        let target_field_occurrence = if first_before_second { 0 } else { 1 };
        let property_occurrence = if first_before_second { 1 } else { 2 };
        let target_field =
            expected_location_signature(&source_path, &source, "FValue", target_field_occurrence);
        let property_accessor =
            expected_location_signature(&source_path, &source, "FValue", property_occurrence);

        let mut server = TestServer::launch();
        server.initialize(temp.path(), Value::Null);

        let references_without_id = RequestId::from(format!("{case_name}-references-without"));
        server.send_request(
            references_without_id.clone(),
            "textDocument/references",
            json!({
                "textDocument": {"uri": uri(&source_path)},
                "position": position_of(&source, "FValue", target_field_occurrence),
                "context": {"includeDeclaration": false}
            }),
        );
        let references_without = result_locations(server.response(&references_without_id));
        assert_exact_location_signatures(&references_without, vec![property_accessor.clone()]);

        let references_with_id = RequestId::from(format!("{case_name}-references-with"));
        server.send_request(
            references_with_id.clone(),
            "textDocument/references",
            json!({
                "textDocument": {"uri": uri(&source_path)},
                "position": position_of(&source, "FValue", target_field_occurrence),
                "context": {"includeDeclaration": true}
            }),
        );
        let references_with = result_locations(server.response(&references_with_id));
        assert_exact_location_signatures(
            &references_with,
            vec![target_field.clone(), property_accessor.clone()],
        );

        let highlights_id = RequestId::from(format!("{case_name}-highlights"));
        server.send_request(
            highlights_id.clone(),
            "textDocument/documentHighlight",
            json!({
                "textDocument": {"uri": uri(&source_path)},
                "position": position_of(&source, "FValue", target_field_occurrence)
            }),
        );
        let highlights = result_locations(server.response(&highlights_id));
        let mut highlight_ranges = highlights.iter().map(range_signature).collect::<Vec<_>>();
        highlight_ranges.sort();
        let mut expected_ranges = [target_field.clone(), property_accessor.clone()]
            .into_iter()
            .map(|(_, line, start, _, end)| (line, start, line, end))
            .collect::<Vec<_>>();
        expected_ranges.sort();
        assert_eq!(highlight_ranges, expected_ranges);

        let rename_id = RequestId::from(format!("{case_name}-rename"));
        server.send_request(
            rename_id.clone(),
            "textDocument/rename",
            json!({
                "textDocument": {"uri": uri(&source_path)},
                "position": position_of(&source, "FValue", target_field_occurrence),
                "newName": "FChanged"
            }),
        );
        let rename = server.response(&rename_id);
        assert!(rename.error.is_none(), "rename failed: {rename:?}");
        assert_exact_workspace_edit(
            &rename.result.expect("rename result"),
            vec![
                (
                    uri(&source_path).to_string(),
                    position_of(&source, "FValue", target_field_occurrence),
                    Position::new(
                        position_of(&source, "FValue", target_field_occurrence).line,
                        position_of(&source, "FValue", target_field_occurrence).character
                            + "FValue".encode_utf16().count() as u32,
                    ),
                    "FChanged".to_owned(),
                ),
                (
                    uri(&source_path).to_string(),
                    position_of(&source, "FValue", property_occurrence),
                    Position::new(
                        position_of(&source, "FValue", property_occurrence).line,
                        position_of(&source, "FValue", property_occurrence).character
                            + "FValue".encode_utf16().count() as u32,
                    ),
                    "FChanged".to_owned(),
                ),
            ],
        );

        assert_eq!(
            fs::read(&source_path).expect("read fixture after queries"),
            disk_source
        );
        server.shutdown();
    }
}

#[test]
fn references_use_unsaved_overlay_text_and_version() {
    let temp = tempfile::tempdir().unwrap();
    let provider = temp.path().join("Provider.pas");
    let consumer = temp.path().join("Consumer.pas");
    let disk_provider = "unit Provider;\ninterface\nconst DiskValue = 1;\nimplementation\nend.\n";
    let overlay_provider = "unit Provider;\ninterface\nconst OverlayValue = 1;\nimplementation\nprocedure Use;\nbegin\n  Log(OverlayValue);\nend;\nend.\n";
    let consumer_source = "unit Consumer;\ninterface\nuses Provider;\nimplementation\nprocedure Run;\nbegin\n  Log(OverlayValue);\nend;\nend.\n";
    write_file(&provider, disk_provider);
    write_file(&consumer, consumer_source);

    let mut server = TestServer::launch();
    server.initialize(temp.path(), Value::Null);
    server.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri(&provider),
                "languageId": "pascal",
                "version": 7,
                "text": overlay_provider
            }
        }),
    );
    let id = RequestId::from("overlay-references".to_string());
    server.send_request(
        id.clone(),
        "textDocument/references",
        json!({
            "textDocument": {"uri": uri(&provider)},
            "position": position_of(overlay_provider, "OverlayValue", 0),
            "context": {"includeDeclaration": false}
        }),
    );
    let response = server.response(&id);
    assert!(response.error.is_none(), "{response:?}");
    let locations = response.result.unwrap().as_array().unwrap().clone();
    assert_eq!(locations.len(), 2);
    assert!(locations.iter().any(|location| {
        location["uri"] == uri(&provider).to_string() && location["range"]["start"]["line"] == 6
    }));
    assert!(locations.iter().any(|location| {
        location["uri"] == uri(&consumer).to_string() && location["range"]["start"]["line"] == 6
    }));
    server.shutdown();
}

#[test]
fn references_and_highlights_follow_overlay_deletions_without_writing_disk_sources() {
    let temp = tempfile::tempdir().unwrap();
    let provider = temp.path().join("Provider.pas");
    let consumer = temp.path().join("Consumer.pas");
    let highlight = temp.path().join("Highlight.pas");
    let provider_source =
        "unit Provider;\ninterface\nconst SharedValue = 1;\nimplementation\nend.\n";
    let consumer_source = "unit Consumer;\ninterface\nuses Provider;\nimplementation\nprocedure Run;\nbegin\n  Log(SharedValue);\nend;\nend.\n";
    let deleted_consumer_source = "unit Consumer;\ninterface\nuses Provider;\nimplementation\nprocedure Run;\nbegin\nend;\nend.\n";
    let highlight_source = "unit Highlight;\ninterface\nconst LocalValue = 1;\nimplementation\nprocedure Run;\nbegin\n  Log(LocalValue);\nend;\nend.\n";
    let deleted_highlight_source = highlight_source.replace("  Log(LocalValue);\n", "");
    write_file(&provider, provider_source);
    write_file(&consumer, consumer_source);
    write_file(&highlight, highlight_source);
    let disk_provider = fs::read(&provider).unwrap();
    let disk_consumer = fs::read(&consumer).unwrap();
    let disk_highlight = fs::read(&highlight).unwrap();

    let mut server = TestServer::launch();
    server.initialize(temp.path(), Value::Null);
    for (path, version, text) in [
        (&consumer, 1, consumer_source),
        (&highlight, 1, highlight_source),
    ] {
        server.send_notification(
            "textDocument/didOpen",
            json!({
                "textDocument": {
                    "uri": uri(path),
                    "languageId": "pascal",
                    "version": version,
                    "text": text
                }
            }),
        );
    }

    let id = RequestId::from("overlay-reference-before-delete".to_string());
    server.send_request(
        id.clone(),
        "textDocument/references",
        json!({
            "textDocument": {"uri": uri(&provider)},
            "position": position_of(provider_source, "SharedValue", 0),
            "context": {"includeDeclaration": false}
        }),
    );
    let response = server.response(&id);
    assert_eq!(result_locations(response).len(), 1);

    let id = RequestId::from("overlay-highlight-before-delete".to_string());
    server.send_request(
        id.clone(),
        "textDocument/documentHighlight",
        json!({
            "textDocument": {"uri": uri(&highlight)},
            "position": position_of(highlight_source, "LocalValue", 0)
        }),
    );
    let response = server.response(&id);
    assert_eq!(result_locations(response).len(), 2);

    server.send_notification(
        "textDocument/didChange",
        json!({
            "textDocument": {"uri": uri(&consumer), "version": 2},
            "contentChanges": [{"text": deleted_consumer_source}]
        }),
    );
    let id = RequestId::from("overlay-reference-after-delete".to_string());
    server.send_request(
        id.clone(),
        "textDocument/references",
        json!({
            "textDocument": {"uri": uri(&provider)},
            "position": position_of(provider_source, "SharedValue", 0),
            "context": {"includeDeclaration": false}
        }),
    );
    let response = server.response(&id);
    assert!(response.error.is_none(), "{response:?}");
    assert!(response.result.unwrap().as_array().unwrap().is_empty());

    server.send_notification(
        "textDocument/didChange",
        json!({
            "textDocument": {"uri": uri(&highlight), "version": 2},
            "contentChanges": [{"text": deleted_highlight_source}]
        }),
    );
    let id = RequestId::from("overlay-highlight-after-delete".to_string());
    server.send_request(
        id.clone(),
        "textDocument/documentHighlight",
        json!({
            "textDocument": {"uri": uri(&highlight)},
            "position": position_of(highlight_source, "LocalValue", 0)
        }),
    );
    let response = server.response(&id);
    let highlights = result_locations(response);
    assert_eq!(highlights.len(), 1);
    assert_eq!(
        highlights[0]["range"],
        json!({
            "start": {"line": 2, "character": 6},
            "end": {"line": 2, "character": 16}
        })
    );

    assert_eq!(fs::read(&provider).unwrap(), disk_provider);
    assert_eq!(fs::read(&consumer).unwrap(), disk_consumer);
    assert_eq!(fs::read(&highlight).unwrap(), disk_highlight);
    server.shutdown();
}

#[test]
fn references_and_highlights_reject_a_removed_selected_project() {
    let temp = tempfile::tempdir().unwrap();
    let main = temp.path().join("Main.pas");
    let selected = temp.path().join("Selected.dproj");
    let remaining = temp.path().join("Remaining.dproj");
    let source = "unit Main;\ninterface\nconst Value = 1;\nimplementation\nprocedure Run;\nbegin\n  Log(Value);\nend;\nend.\n";
    write_file(&main, source);
    for project in [&selected, &remaining] {
        write_file(
            project,
            "<Project><PropertyGroup><MainSource>Main.pas</MainSource></PropertyGroup></Project>",
        );
    }

    let mut server = TestServer::launch();
    server.initialize(temp.path(), Value::Null);
    let select_id = RequestId::from("select-project-for-queries".to_string());
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

    let references_id = RequestId::from("removed-project-references".to_string());
    server.send_request(
        references_id.clone(),
        "textDocument/references",
        json!({
            "textDocument": {"uri": uri(&main)},
            "position": position_of(source, "Value", 0),
            "context": {"includeDeclaration": true}
        }),
    );
    let references = server.response(&references_id);
    let error = references
        .error
        .expect("references must reject a removed selected project");
    assert!(error.message.contains("project selection") || error.message.contains("invalid"));
    assert!(references.result.is_none());

    let highlights_id = RequestId::from("removed-project-highlights".to_string());
    server.send_request(
        highlights_id.clone(),
        "textDocument/documentHighlight",
        json!({
            "textDocument": {"uri": uri(&main)},
            "position": position_of(source, "Value", 0)
        }),
    );
    let highlights = server.response(&highlights_id);
    let error = highlights
        .error
        .expect("highlights must reject a removed selected project");
    assert!(error.message.contains("project selection") || error.message.contains("invalid"));
    assert!(highlights.result.is_none());
    server.shutdown();
}

#[test]
fn references_respect_explicit_source_exclusions() {
    let temp = tempfile::tempdir().unwrap();
    let provider = temp.path().join("Provider.pas");
    let consumer = temp.path().join("excluded/Consumer.pas");
    let provider_source =
        "unit Provider;\ninterface\nconst SharedValue = 1;\nimplementation\nend.\n";
    let consumer_source = "unit Consumer;\ninterface\nuses Provider;\nimplementation\nprocedure Run;\nbegin\n  Log(SharedValue);\nend;\nend.\n";
    write_file(&provider, provider_source);
    write_file(&consumer, consumer_source);

    let mut server = TestServer::launch();
    server.initialize(temp.path(), json!({"exclude": ["excluded/**"]}));
    let id = RequestId::from("excluded-reference-consumer".to_string());
    server.send_request(
        id.clone(),
        "textDocument/references",
        json!({
            "textDocument": {"uri": uri(&provider)},
            "position": position_of(provider_source, "SharedValue", 0),
            "context": {"includeDeclaration": false}
        }),
    );
    let response = server.response(&id);
    assert!(response.error.is_none(), "{response:?}");
    assert!(response.result.unwrap().as_array().unwrap().is_empty());
    server.shutdown();
}

#[test]
fn document_highlights_are_local_and_include_declaration() {
    let temp = tempfile::tempdir().unwrap();
    let provider = temp.path().join("Provider.pas");
    let consumer = temp.path().join("Consumer.pas");
    let provider_source = "unit Provider;\ninterface\nconst SharedValue = 1;\nimplementation\nprocedure Run;\nbegin\n  Log('😀', SharedValue);\n  Log('😀', SharedValue);\nend;\nend.\n";
    let consumer_source = "unit Consumer;\ninterface\nuses Provider;\nimplementation\nprocedure Use;\nbegin\n  Log(SharedValue);\nend;\nend.\n";
    write_file(&provider, provider_source);
    write_file(&consumer, consumer_source);

    let mut server = TestServer::launch();
    server.initialize(temp.path(), Value::Null);
    let id = RequestId::from("document-highlights".to_string());
    server.send_request(
        id.clone(),
        "textDocument/documentHighlight",
        json!({
            "textDocument": {"uri": uri(&provider)},
            "position": position_of(provider_source, "SharedValue", 0)
        }),
    );
    let response = server.response(&id);
    assert!(response.error.is_none(), "{response:?}");
    let highlights = response.result.unwrap().as_array().unwrap().clone();
    assert_eq!(highlights.len(), 3);
    assert!(highlights.iter().all(|highlight| {
        highlight["uri"].is_null() && highlight["kind"].is_null() && highlight["range"].is_object()
    }));
    let starts = highlights
        .iter()
        .map(|highlight| {
            (
                highlight["range"]["start"]["line"].as_u64().unwrap(),
                highlight["range"]["start"]["character"].as_u64().unwrap(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(starts, vec![(2, 6), (6, 12), (7, 12)]);
    assert!(!highlights.iter().any(|highlight| {
        highlight["range"]["start"]["line"] == 7
            && highlight["range"]["start"]["character"] == 6
            && highlight["uri"] == uri(&consumer).to_string()
    }));
    server.shutdown();
}

#[test]
fn document_highlights_retain_needed_import_bindings_without_consumer_scan() {
    let temp = tempfile::tempdir().unwrap();
    let provider = temp.path().join("Provider.pas");
    let consumer = temp.path().join("Consumer.pas");
    let provider_source =
        "unit Provider;\ninterface\nconst SharedValue = 1;\nimplementation\nend.\n";
    let consumer_source = "unit Consumer;\ninterface\nuses Provider;\nimplementation\nprocedure Use;\nbegin\n  Log(SharedValue);\nend;\nend.\n";
    write_file(&provider, provider_source);
    write_file(&consumer, consumer_source);

    let mut server = TestServer::launch();
    server.initialize(temp.path(), Value::Null);
    let id = RequestId::from("imported-document-highlights".to_string());
    server.send_request(
        id.clone(),
        "textDocument/documentHighlight",
        json!({
            "textDocument": {"uri": uri(&consumer)},
            "position": position_of(consumer_source, "SharedValue", 0)
        }),
    );
    let response = server.response(&id);
    assert!(response.error.is_none(), "{response:?}");
    let highlights = response.result.unwrap().as_array().unwrap().clone();
    assert_eq!(highlights.len(), 1);
    assert_eq!(
        highlights[0]["range"],
        json!({
            "start": {"line": 6, "character": 6},
            "end": {"line": 6, "character": 17}
        })
    );
    server.shutdown();
}

#[test]
fn unit_module_reference_queries_fail_and_unit_highlights_are_empty() {
    let temp = tempfile::tempdir().unwrap();
    let source_path = temp.path().join("ReviewUnit.pas");
    let source = "unit ReviewUnit;\ninterface\nimplementation\nend.\n";
    write_file(&source_path, source);

    let mut server = TestServer::launch();
    server.initialize(temp.path(), Value::Null);

    let references_id = RequestId::from("unit-module-references".to_string());
    server.send_request(
        references_id.clone(),
        "textDocument/references",
        json!({
            "textDocument": {"uri": uri(&source_path)},
            "position": position_of(source, "ReviewUnit", 0),
            "context": {"includeDeclaration": true}
        }),
    );
    let references = server.response(&references_id);
    let error = references
        .error
        .expect("unit/module references are outside the supported binding subset");
    assert_eq!(error.code, -32803);
    assert!(error.message.contains("unit/module"));
    assert!(references.result.is_none());

    let highlights_id = RequestId::from("unit-module-highlights".to_string());
    server.send_request(
        highlights_id.clone(),
        "textDocument/documentHighlight",
        json!({
            "textDocument": {"uri": uri(&source_path)},
            "position": position_of(source, "ReviewUnit", 0)
        }),
    );
    let highlights = server.response(&highlights_id);
    assert!(highlights.error.is_none(), "{highlights:?}");
    assert!(highlights.result.unwrap().as_array().unwrap().is_empty());
    server.shutdown();
}

#[test]
fn document_highlights_ignore_huge_unreadable_unrelated_trees_and_dependency_occurrences() {
    let temp = tempfile::tempdir().unwrap();
    let provider = temp.path().join("Provider.pas");
    let main = temp.path().join("Main.pas");
    let unrelated = temp.path().join("unrelated/Huge.pas");
    let unreadable = temp.path().join("unrelated/Unreadable.pas");
    let mut provider_source = String::from(
        "unit Provider;\ninterface\nconst SharedValue = 1;\nimplementation\nprocedure Noise;\nbegin\n",
    );
    for _ in 0..10_000 {
        provider_source.push_str("  Log(SharedValue);\n");
    }
    provider_source.push_str("end;\nend.\n");
    let main_source = "unit Main;\ninterface\nuses Provider;\nimplementation\nprocedure Run;\nbegin\n  Log(SharedValue);\nend;\nend.\n";
    let huge_unrelated = format!(
        "unit Huge;\ninterface\nconst SharedValue = 1;\nimplementation\n{}\n",
        "x".repeat(2 * 1024 * 1024)
    );
    let unreadable_source =
        "unit Unreadable;\ninterface\nconst SharedValue = 1;\nimplementation\nend.\n";
    write_file(&provider, &provider_source);
    write_file(&main, main_source);
    write_file(&unrelated, &huge_unrelated);
    write_file(&unreadable, unreadable_source);
    #[cfg(unix)]
    {
        let mut permissions = fs::metadata(&unreadable).unwrap().permissions();
        permissions.set_mode(0o000);
        fs::set_permissions(&unreadable, permissions).unwrap();
    }

    let mut server = TestServer::launch();
    server.initialize(temp.path(), Value::Null);
    let id = RequestId::from("isolated-highlight-tree".to_string());
    server.send_request(
        id.clone(),
        "textDocument/documentHighlight",
        json!({
            "textDocument": {"uri": uri(&main)},
            "position": position_of(main_source, "SharedValue", 0)
        }),
    );
    let response = server.response(&id);
    assert!(response.error.is_none(), "{response:?}");
    let highlights = response.result.unwrap().as_array().unwrap().clone();
    assert_eq!(
        highlights.len(),
        1,
        "dependency occurrences and unrelated unreadable/huge files must not affect local highlights"
    );
    assert_eq!(
        highlights[0]["range"],
        json!({
            "start": {"line": 6, "character": 6},
            "end": {"line": 6, "character": 17}
        })
    );
    server.shutdown();
}

#[test]
fn document_highlights_reject_incomplete_import_binding_snapshots() {
    let temp = tempfile::tempdir().unwrap();
    let main = temp.path().join("Main.pas");
    let provider_a = temp.path().join("ProviderA.pas");
    let provider_b = temp.path().join("ProviderB.pas");
    let main_source = "unit Main;\ninterface\nuses ProviderA, ProviderB;\nimplementation\nprocedure Run;\nbegin\n  Log(SharedValue);\nend;\nend.\n";
    let provider_source =
        "unit ProviderA;\ninterface\nconst SharedValue = 1;\nimplementation\nend.\n";
    let provider_b_source = provider_source.replace("ProviderA", "ProviderB");
    write_file(&main, main_source);
    write_file(&provider_a, provider_source);
    write_file(&provider_b, &provider_b_source);

    let request = json!({
        "textDocument": {"uri": uri(&main)},
        "position": position_of(main_source, "SharedValue", 0)
    });
    let mut incomplete_server = TestServer::launch();
    incomplete_server.initialize(temp.path(), json!({"maxFiles": 2}));
    let incomplete_id = RequestId::from("incomplete-import-highlights".to_string());
    incomplete_server.send_request(
        incomplete_id.clone(),
        "textDocument/documentHighlight",
        request.clone(),
    );
    let incomplete = incomplete_server.response(&incomplete_id);
    assert!(
        incomplete.error.is_some(),
        "incomplete import binding must not guess the retained provider: {incomplete:?}"
    );
    assert!(incomplete.result.is_none());
    incomplete_server.shutdown();

    let mut complete_server = TestServer::launch();
    complete_server.initialize(temp.path(), json!({"maxFiles": 3}));
    let complete_id = RequestId::from("complete-import-highlights".to_string());
    complete_server.send_request(
        complete_id.clone(),
        "textDocument/documentHighlight",
        request,
    );
    let complete = complete_server.response(&complete_id);
    assert!(complete.error.is_none(), "{complete:?}");
    assert!(complete.result.unwrap().as_array().unwrap().is_empty());
    complete_server.shutdown();
}

#[test]
fn reference_and_highlight_queries_return_empty_for_whitespace() {
    let temp = tempfile::tempdir().unwrap();
    let source_path = temp.path().join("WhitespaceQueries.pas");
    let source = "unit WhitespaceQueries;\ninterface\nconst Value = 1;\nimplementation\nprocedure Run;\nbegin\n  // Value\n  Log('Value');\n  Value;\nend;\nend.\n";
    write_file(&source_path, source);

    let mut server = TestServer::launch();
    server.initialize(temp.path(), Value::Null);
    for (id_text, method, params) in [
        (
            "whitespace-references",
            "textDocument/references",
            json!({
                "textDocument": {"uri": uri(&source_path)},
                "position": {"line": 5, "character": 5},
                "context": {"includeDeclaration": true}
            }),
        ),
        (
            "whitespace-highlights",
            "textDocument/documentHighlight",
            json!({
                "textDocument": {"uri": uri(&source_path)},
                "position": {"line": 5, "character": 5}
            }),
        ),
        (
            "comment-references",
            "textDocument/references",
            json!({
                "textDocument": {"uri": uri(&source_path)},
                "position": position_of(source, "Value", 1),
                "context": {"includeDeclaration": true}
            }),
        ),
        (
            "comment-highlights",
            "textDocument/documentHighlight",
            json!({
                "textDocument": {"uri": uri(&source_path)},
                "position": position_of(source, "Value", 1)
            }),
        ),
        (
            "string-references",
            "textDocument/references",
            json!({
                "textDocument": {"uri": uri(&source_path)},
                "position": position_of(source, "Value", 2),
                "context": {"includeDeclaration": true}
            }),
        ),
        (
            "string-highlights",
            "textDocument/documentHighlight",
            json!({
                "textDocument": {"uri": uri(&source_path)},
                "position": position_of(source, "Value", 2)
            }),
        ),
    ] {
        let id = RequestId::from(id_text.to_string());
        server.send_request(id.clone(), method, params);
        let response = server.response(&id);
        assert!(response.error.is_none(), "{response:?}");
        assert!(response.result.unwrap().as_array().unwrap().is_empty());
    }
    server.shutdown();
}

#[test]
fn highlights_ignore_an_unrelated_unsupported_same_named_binding() {
    let temp = tempfile::tempdir().unwrap();
    let source_path = temp.path().join("UnsupportedHighlight.pas");
    let source = "unit UnsupportedHighlight;\ninterface\nconst Target = 1;\nimplementation\nprocedure Run;\nbegin\n  Log(Target);\n  with Unknown do\n    Target := 2;\nend;\nend.\n";
    write_file(&source_path, source);

    let mut server = TestServer::launch();
    server.initialize(temp.path(), Value::Null);
    let id = RequestId::from("unsupported-highlight".to_string());
    server.send_request(
        id.clone(),
        "textDocument/documentHighlight",
        json!({
            "textDocument": {"uri": uri(&source_path)},
            "position": position_of(source, "Target", 0)
        }),
    );
    let response = server.response(&id);
    assert!(response.error.is_none(), "{response:?}");
    let result = response.result.unwrap();
    let highlights = result.as_array().unwrap();
    assert_eq!(highlights.len(), 2);
    assert!(highlights.iter().all(|highlight| {
        highlight["range"]["start"]["line"] == 2 || highlight["range"]["start"]["line"] == 6
    }));

    let id = RequestId::from("unsupported-highlight-selected-with".to_string());
    server.send_request(
        id.clone(),
        "textDocument/documentHighlight",
        json!({
            "textDocument": {"uri": uri(&source_path)},
            "position": position_of(source, "Target", 2)
        }),
    );
    let response = server.response(&id);
    assert!(response.error.is_none(), "{response:?}");
    assert!(
        response.result.unwrap().as_array().unwrap().is_empty(),
        "an unsupported selected with occurrence must not authorize global highlights"
    );
    server.shutdown();
}

#[test]
fn document_highlights_return_empty_for_a_selected_unknown_ancestor_fallback() {
    let temp = tempfile::tempdir().unwrap();
    let source_path = temp.path().join("UnknownAncestorHighlight.pas");
    let source = "unit UnknownAncestorHighlight;\ninterface\ntype\n  TChild = class(TUnknownAncestor)\n  public\n    procedure Use;\n  end;\nconst\n  Target = 1;\nimplementation\nprocedure TChild.Use;\nbegin\n  Log(Target);\nend;\nend.\n";
    write_file(&source_path, source);

    let mut server = TestServer::launch();
    server.initialize(temp.path(), Value::Null);
    let id = RequestId::from("unknown-ancestor-highlight".to_string());
    server.send_request(
        id.clone(),
        "textDocument/documentHighlight",
        json!({
            "textDocument": {"uri": uri(&source_path)},
            "position": position_of(source, "Target", 1)
        }),
    );
    let response = server.response(&id);
    assert!(response.error.is_none(), "{response:?}");
    assert!(
        response.result.unwrap().as_array().unwrap().is_empty(),
        "an unknown ancestor occurrence must not authorize global highlights"
    );
    server.shutdown();
}

#[test]
fn document_highlights_return_empty_for_an_unbound_cursor() {
    let temp = tempfile::tempdir().unwrap();
    let source_path = temp.path().join("UnboundHighlight.pas");
    let source = "unit UnboundHighlight;\ninterface\nimplementation\nprocedure Run;\nbegin\n  Unknown;\nend;\nend.\n";
    write_file(&source_path, source);

    let mut server = TestServer::launch();
    server.initialize(temp.path(), Value::Null);
    let id = RequestId::from("unbound-highlight".to_string());
    server.send_request(
        id.clone(),
        "textDocument/documentHighlight",
        json!({
            "textDocument": {"uri": uri(&source_path)},
            "position": position_of(source, "Unknown", 0)
        }),
    );
    let response = server.response(&id);
    assert!(response.error.is_none(), "{response:?}");
    assert!(response.result.unwrap().as_array().unwrap().is_empty());
    server.shutdown();
}

#[test]
fn references_reject_an_incomplete_workspace_without_partial_locations() {
    let temp = tempfile::tempdir().unwrap();
    let provider = temp.path().join("Provider.pas");
    let consumer = temp.path().join("Consumer.pas");
    let provider_source =
        "unit Provider;\ninterface\nconst SharedValue = 1;\nimplementation\nend.\n";
    let consumer_source = "unit Consumer;\ninterface\nuses MissingUnit;\nimplementation\nprocedure Run;\nbegin\n  Log(SharedValue);\nend;\nend.\n";
    write_file(&provider, provider_source);
    write_file(&consumer, consumer_source);

    let mut server = TestServer::launch();
    server.initialize(temp.path(), Value::Null);
    let id = RequestId::from("incomplete-references".to_string());
    server.send_request(
        id.clone(),
        "textDocument/references",
        json!({
            "textDocument": {"uri": uri(&provider)},
            "position": position_of(provider_source, "SharedValue", 0),
            "context": {"includeDeclaration": false}
        }),
    );
    let response = server.response(&id);
    let error = response
        .error
        .expect("incomplete reference discovery must fail closed");
    assert!(error.message.contains("incomplete"));
    assert!(response.result.is_none());
    server.shutdown();
}

#[test]
fn references_report_the_actual_response_bound() {
    let temp = tempfile::tempdir().unwrap();
    let source_path = temp.path().join("ManyReferences.pas");
    let mut source = String::from(
        "unit ManyReferences;\ninterface\nconst Value = 1;\nimplementation\nprocedure Run;\nbegin\n",
    );
    for _ in 0..10_001 {
        source.push_str("  Log(Value);\n");
    }
    source.push_str("end;\nend.\n");
    write_file(&source_path, &source);

    let mut server = TestServer::launch();
    server.initialize(temp.path(), Value::Null);
    let id = RequestId::from("reference-bound".to_string());
    server.send_request(
        id.clone(),
        "textDocument/references",
        json!({
            "textDocument": {"uri": uri(&source_path)},
            "position": position_of(&source, "Value", 0),
            "context": {"includeDeclaration": false}
        }),
    );
    let response = server.response(&id);
    let error = response
        .error
        .expect("reference result bound must fail closed");
    assert!(error.message.contains("10000"), "{error:?}");

    let id = RequestId::from("highlight-bound".to_string());
    server.send_request(
        id.clone(),
        "textDocument/documentHighlight",
        json!({
            "textDocument": {"uri": uri(&source_path)},
            "position": position_of(&source, "Value", 0)
        }),
    );
    let response = server.response(&id);
    let error = response
        .error
        .expect("document highlight result bound must fail closed");
    assert!(error.message.contains("10000"), "{error:?}");
    server.shutdown();
}

#[test]
fn references_and_highlights_enforce_exact_10000_entry_boundaries() {
    let temp = tempfile::tempdir().unwrap();
    let provider = temp.path().join("Provider.pas");
    let consumer = temp.path().join("Consumer.pas");
    let consumer_two = temp.path().join("ConsumerTwo.pas");
    let highlight_exact = temp.path().join("HighlightExact.pas");
    let highlight_overflow = temp.path().join("HighlightOverflow.pas");
    let provider_source =
        "unit Provider;\ninterface\nconst SharedValue = 1;\nimplementation\nend.\n";
    let make_consumer = |unit: &str, uses: usize| {
        let mut source = format!(
            "unit {unit};\ninterface\nuses Provider;\nimplementation\nprocedure Run;\nbegin\n"
        );
        for _ in 0..uses {
            source.push_str("  Log(SharedValue);\n");
        }
        source.push_str("end;\nend.\n");
        source
    };
    let make_highlight_source = |unit: &str, uses: usize| {
        let mut source = format!(
            "unit {unit};\ninterface\nconst Target = 1;\nimplementation\nprocedure Run;\nbegin\n"
        );
        for _ in 0..uses {
            source.push_str("  Log(Target);\n");
        }
        source.push_str("end;\nend.\n");
        source
    };
    let consumer_exact = make_consumer("Consumer", 5_000);
    let consumer_two_exact = make_consumer("ConsumerTwo", 5_000);
    let highlight_exact_source = make_highlight_source("HighlightExact", 9_999);
    let highlight_overflow_source = make_highlight_source("HighlightOverflow", 10_000);
    write_file(&provider, provider_source);
    write_file(&consumer, &consumer_exact);
    write_file(&consumer_two, &consumer_two_exact);
    write_file(&highlight_exact, &highlight_exact_source);
    write_file(&highlight_overflow, &highlight_overflow_source);

    let mut server = TestServer::launch();
    server.initialize(temp.path(), Value::Null);

    let id = RequestId::from("references-exact-bound".to_string());
    server.send_request(
        id.clone(),
        "textDocument/references",
        json!({
            "textDocument": {"uri": uri(&provider)},
            "position": position_of(provider_source, "SharedValue", 0),
            "context": {"includeDeclaration": false}
        }),
    );
    let response = server.response(&id);
    assert!(response.error.is_none(), "{response:?}");
    let locations = response.result.unwrap().as_array().unwrap().clone();
    assert_eq!(locations.len(), 10_000);
    assert!(
        locations
            .iter()
            .filter(|location| location["uri"] == uri(&consumer).to_string())
            .count()
            == 5_000
    );
    assert_eq!(
        locations
            .iter()
            .filter(|location| location["uri"] == uri(&consumer_two).to_string())
            .count(),
        5_000
    );

    let consumer_overflow = make_consumer("Consumer", 5_001);
    write_file(&consumer, &consumer_overflow);
    let id = RequestId::from("references-overflow-bound".to_string());
    server.send_request(
        id.clone(),
        "textDocument/references",
        json!({
            "textDocument": {"uri": uri(&provider)},
            "position": position_of(provider_source, "SharedValue", 0),
            "context": {"includeDeclaration": false}
        }),
    );
    let response = server.response(&id);
    let error = response
        .error
        .expect("10,001 reference locations must fail closed");
    assert_eq!(
        error.message,
        "binding reference result exceeds the 10000-entry limit"
    );
    assert!(response.result.is_none());

    let id = RequestId::from("highlights-exact-bound".to_string());
    server.send_request(
        id.clone(),
        "textDocument/documentHighlight",
        json!({
            "textDocument": {"uri": uri(&highlight_exact)},
            "position": position_of(&highlight_exact_source, "Target", 0)
        }),
    );
    let response = server.response(&id);
    assert!(response.error.is_none(), "{response:?}");
    let highlights = response.result.unwrap().as_array().unwrap().clone();
    assert_eq!(highlights.len(), 10_000);
    assert_eq!(highlights[0]["range"]["start"]["line"], 2);

    let id = RequestId::from("highlights-overflow-bound".to_string());
    server.send_request(
        id.clone(),
        "textDocument/documentHighlight",
        json!({
            "textDocument": {"uri": uri(&highlight_overflow)},
            "position": position_of(&highlight_overflow_source, "Target", 0)
        }),
    );
    let response = server.response(&id);
    let error = response
        .error
        .expect("10,001 highlight locations must fail closed");
    assert_eq!(
        error.message,
        "binding reference result exceeds the 10000-entry limit"
    );
    assert!(response.result.is_none());
    server.shutdown();
}

#[test]
fn reference_cancellation_handles_thousands_of_occurrences() {
    let temp = tempfile::tempdir().unwrap();
    let source_path = temp.path().join("ManyReferenceUses.pas");
    let mut source = String::from(
        "unit ManyReferenceUses;\ninterface\nconst Value = 1;\nimplementation\nprocedure Run;\nbegin\n",
    );
    for _ in 0..4_000 {
        source.push_str("  Log(Value);\n");
    }
    source.push_str("end;\nend.\n");
    write_file(&source_path, &source);

    let mut server = TestServer::launch();
    server.initialize(temp.path(), Value::Null);
    let id = RequestId::from("cancelled-references".to_string());
    server.send_request(
        id.clone(),
        "textDocument/references",
        json!({
            "textDocument": {"uri": uri(&source_path)},
            "position": position_of(&source, "Value", 0),
            "context": {"includeDeclaration": false}
        }),
    );
    server.send_notification("$/cancelRequest", json!({"id": "cancelled-references"}));
    let response = server.response(&id);
    let error = response.error.expect("cancelled reference query must fail");
    assert_eq!(error.code, -32800);
    assert_eq!(error.message, "request cancelled");
    server.shutdown();
}

#[test]
fn hover_cancellation_handles_an_in_flight_large_source_request() {
    let temp = tempfile::tempdir().unwrap();
    let source_path = temp.path().join("ManyHoverUses.pas");
    let mut source = String::from(
        "unit ManyHoverUses;\ninterface\nconst Value = 1;\nimplementation\nprocedure Run;\nbegin\n",
    );
    for _ in 0..20_000 {
        source.push_str("  Log(Value);\n");
    }
    source.push_str("end;\nend.\n");
    write_file(&source_path, &source);

    let mut server = TestServer::launch();
    server.initialize(temp.path(), Value::Null);
    let id = RequestId::from("cancelled-hover".to_string());
    server.send_request(
        id.clone(),
        "textDocument/hover",
        navigation_params(&source_path, &source, "Value", 0),
    );
    server.send_notification("$/cancelRequest", json!({"id": "cancelled-hover"}));
    let response = server.response(&id);
    let error = response.error.expect("cancelled hover query must fail");
    assert_eq!(error.code, -32800);
    assert_eq!(error.message, "request cancelled");
    server.shutdown();
}

#[test]
fn completion_cancellation_handles_an_in_flight_large_source_request() {
    let temp = tempfile::tempdir().unwrap();
    let source_path = temp.path().join("ManyCompletionUses.pas");
    let mut source = String::from(
        "unit ManyCompletionUses;\ninterface\nconst Value = 1;\nimplementation\nprocedure Run;\nbegin\n",
    );
    for _ in 0..20_000 {
        source.push_str("  Log(Value);\n");
    }
    source.push_str("  Va;\nend;\nend.\n");
    write_file(&source_path, &source);

    let mut server = TestServer::launch();
    server.initialize(temp.path(), Value::Null);
    let id = RequestId::from("cancelled-completion".to_string());
    let completion_start = position_of(&source, "  Va;", 0);
    server.send_request(
        id.clone(),
        "textDocument/completion",
        json!({
            "textDocument": {"uri": uri(&source_path)},
            "position": Position::new(completion_start.line, completion_start.character + 4)
        }),
    );
    server.send_notification("$/cancelRequest", json!({"id": "cancelled-completion"}));
    let response = server.response(&id);
    let error = response
        .error
        .expect("cancelled completion query must fail");
    assert_eq!(error.code, -32800);
    assert_eq!(error.message, "request cancelled");
    server.shutdown();
}

#[test]
fn signature_help_cancellation_handles_an_in_flight_large_source_request() {
    let temp = tempfile::tempdir().unwrap();
    let source_path = temp.path().join("ManySignatureUses.pas");
    let mut source = String::from(
        "unit ManySignatureUses;\ninterface\nprocedure Run(Value: Integer);\nimplementation\nprocedure Run(Value: Integer);\nbegin\nend;\nprocedure Caller;\nbegin\n",
    );
    for _ in 0..20_000 {
        source.push_str("  Log(1);\n");
    }
    source.push_str("  Run(1);\nend;\nend.\n");
    write_file(&source_path, &source);

    let mut server = TestServer::launch();
    server.initialize(temp.path(), Value::Null);
    let id = RequestId::from("cancelled-signature-help".to_string());
    let call_start = position_of(&source, "  Run(1);", 0);
    server.send_request(
        id.clone(),
        "textDocument/signatureHelp",
        json!({
            "textDocument": {"uri": uri(&source_path)},
            "position": Position::new(call_start.line, call_start.character + 6)
        }),
    );
    server.send_notification("$/cancelRequest", json!({"id": "cancelled-signature-help"}));
    let response = server.response(&id);
    let error = response
        .error
        .expect("cancelled signature-help query must fail");
    assert_eq!(error.code, -32800);
    assert_eq!(error.message, "request cancelled");
    server.shutdown();
}

#[test]
fn workspace_symbols_respect_exclusions_and_unsaved_overlays() {
    let temp = tempfile::tempdir().unwrap();
    let visible = temp.path().join("Visible.pas");
    let excluded_dir = temp.path().join("ignored");
    let excluded = excluded_dir.join("Excluded.pas");
    fs::create_dir_all(&excluded_dir).unwrap();
    write_file(
        &visible,
        "unit Visible;\ninterface\nprocedure DiskOnly;\nimplementation\nend.\n",
    );
    write_file(
        &excluded,
        "unit Excluded;\ninterface\nprocedure HiddenThing;\nimplementation\nend.\n",
    );
    let mut server = TestServer::launch();
    server.initialize(temp.path(), json!({"exclude": ["ignored"]}));
    server.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri(&visible),
                "languageId": "pascal",
                "version": 1,
                "text": "unit Visible;\ninterface\nprocedure OverlayOnly;\nimplementation\nend.\n"
            }
        }),
    );

    let id = RequestId::from("workspace-symbols-overlay".to_string());
    server.send_request(id.clone(), "workspace/symbol", json!({"query": "Only"}));
    let response = server.response(&id);
    assert!(response.error.is_none(), "{:?}", response.error);
    let symbols = response.result.unwrap().as_array().unwrap().clone();
    assert_eq!(
        symbols
            .iter()
            .map(|symbol| symbol["name"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["OverlayOnly"]
    );

    let id = RequestId::from("workspace-symbols-excluded".to_string());
    server.send_request(
        id.clone(),
        "workspace/symbol",
        json!({"query": "HiddenThing"}),
    );
    let response = server.response(&id);
    assert!(response.error.is_none(), "{:?}", response.error);
    assert!(response.result.unwrap().as_array().unwrap().is_empty());
    server.shutdown();
}

#[test]
fn symbol_queries_allow_readable_external_source_paths() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("workspace");
    let external = temp.path().join("external-sources");
    let main = root.join("Main.pas");
    let external_source = external.join("External.pas");
    write_file(&main, "unit Main;\ninterface\nimplementation\nend.\n");
    let external_text =
        "unit External;\ninterface\nprocedure ExternalThing;\nimplementation\nend.\n";
    write_file(&external_source, external_text);

    let mut server = TestServer::launch();
    server.initialize(
        &root,
        json!({"sourcePaths": [external.to_string_lossy().to_string()]}),
    );

    let outline_id = RequestId::from("external-outline".to_string());
    server.send_request(
        outline_id.clone(),
        "textDocument/documentSymbol",
        json!({"textDocument": {"uri": uri(&external_source)}}),
    );
    let outline = server.response(&outline_id);
    assert!(
        outline.error.is_none(),
        "external outline failed: {outline:?}"
    );
    assert_eq!(outline.result.unwrap()[0]["name"], "External");

    let search_id = RequestId::from("external-search".to_string());
    server.send_request(
        search_id.clone(),
        "workspace/symbol",
        json!({"query": "ExternalThing"}),
    );
    let search = server.response(&search_id);
    assert!(search.error.is_none(), "external search failed: {search:?}");
    assert_eq!(search.result.unwrap()[0]["name"], "ExternalThing");

    let references_id = RequestId::from("external-references".to_string());
    server.send_request(
        references_id.clone(),
        "textDocument/references",
        json!({
            "textDocument": {"uri": uri(&external_source)},
            "position": position_of(external_text, "ExternalThing", 0),
            "context": {"includeDeclaration": true}
        }),
    );
    let references = server.response(&references_id);
    assert!(
        references.error.is_none(),
        "external references failed: {references:?}"
    );
    assert_eq!(references.result.unwrap().as_array().unwrap().len(), 1);

    let highlights_id = RequestId::from("external-highlights".to_string());
    server.send_request(
        highlights_id.clone(),
        "textDocument/documentHighlight",
        json!({
            "textDocument": {"uri": uri(&external_source)},
            "position": position_of(external_text, "ExternalThing", 0)
        }),
    );
    let highlights = server.response(&highlights_id);
    assert!(
        highlights.error.is_none(),
        "external highlights failed: {highlights:?}"
    );
    assert_eq!(highlights.result.unwrap().as_array().unwrap().len(), 1);
    server.shutdown();
}

#[test]
fn symbol_queries_require_source_membership_for_external_documents() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("workspace");
    let external = temp.path().join("external-sources");
    let main = root.join("Main.pas");
    let external_source = external.join("External.pas");
    write_file(&main, "unit Main; interface implementation end.\n");
    write_file(
        &external_source,
        "unit External; interface procedure ExternalThing; implementation end.\n",
    );

    let mut server = TestServer::launch();
    server.initialize(&root, Value::Null);

    let outline_id = RequestId::from("unconfigured-external-outline".to_string());
    server.send_request(
        outline_id.clone(),
        "textDocument/documentSymbol",
        json!({"textDocument": {"uri": uri(&external_source)}}),
    );
    let outline = server.response(&outline_id);
    let error = outline
        .error
        .expect("unconfigured external outline must fail");
    assert!(error.message.contains("outside configured workspace roots"));

    let search_id = RequestId::from("unconfigured-external-search".to_string());
    server.send_request(
        search_id.clone(),
        "workspace/symbol",
        json!({"query": "ExternalThing"}),
    );
    let search = server.response(&search_id);
    assert!(
        search.error.is_none(),
        "workspace search failed: {search:?}"
    );
    assert!(search.result.unwrap().as_array().unwrap().is_empty());
    server.shutdown();
}

#[test]
fn structural_symbol_queries_ignore_unresolved_include_dependencies_in_ambiguous_project_context() {
    let temp = tempfile::tempdir().unwrap();
    let source_path = temp.path().join("Main.pas");
    write_file(
        &source_path,
        "unit Main;\ninterface\nprocedure VisibleThing;\n{$I missing.inc}\nimplementation\nend.\n",
    );
    for name in ["A", "B"] {
        write_file(
            &temp.path().join(format!("{name}.dproj")),
            "<Project><PropertyGroup><MainSource>Main.pas</MainSource></PropertyGroup></Project>",
        );
    }
    let mut server = TestServer::launch();
    server.initialize(temp.path(), Value::Null);

    let id = RequestId::from("structural-symbols".to_string());
    server.send_request(
        id.clone(),
        "workspace/symbol",
        json!({"query": "VisibleThing"}),
    );
    let response = server.response(&id);
    assert!(
        response.error.is_none(),
        "structural search failed: {response:?}"
    );
    assert_eq!(response.result.unwrap()[0]["name"], "VisibleThing");
    server.shutdown();
}

#[test]
fn workspace_symbol_queries_report_the_actual_file_count_bound() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("workspace");
    write_file(
        &root.join("First.pas"),
        "unit First; interface procedure FirstThing; implementation end.\n",
    );
    write_file(
        &root.join("Second.pas"),
        "unit Second; interface procedure SecondThing; implementation end.\n",
    );

    let mut server = TestServer::launch();
    server.initialize(&root, json!({"maxFiles": 1}));
    let id = RequestId::from("file-count-bound".to_string());
    server.send_request(
        id.clone(),
        "workspace/symbol",
        json!({"query": "SecondThing"}),
    );
    let response = server.response(&id);
    let error = response
        .error
        .expect("workspace symbol file bound must fail closed");
    assert!(error.message.contains("incomplete"));
    assert!(error.message.contains("file limit"));
    server.shutdown();
}

#[test]
fn workspace_symbol_queries_report_the_actual_total_byte_bound() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("workspace");
    let source = "unit Bounded;\ninterface\nprocedure BoundedThing;\nimplementation\nend.\n";
    write_file(&root.join("First.pas"), source);
    write_file(&root.join("Second.pas"), source);

    let mut server = TestServer::launch();
    server.initialize(
        &root,
        json!({"maxFiles": 4, "maxTotalBytes": source.len() + 1}),
    );
    let id = RequestId::from("total-byte-bound".to_string());
    server.send_request(
        id.clone(),
        "workspace/symbol",
        json!({"query": "BoundedThing"}),
    );
    let response = server.response(&id);
    let error = response
        .error
        .expect("workspace symbol total byte bound must fail closed");
    assert!(error.message.contains("incomplete"));
    assert!(error.message.contains("byte limit"));
    server.shutdown();
}

#[test]
fn workspace_symbols_accept_exactly_the_response_bound_across_files() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("workspace");
    write_file(
        &root.join("First.pas"),
        &workspace_symbol_source("First", 4_999),
    );
    write_file(
        &root.join("Second.pas"),
        &workspace_symbol_source("Second", 4_999),
    );

    let mut server = TestServer::launch();
    server.initialize(&root, Value::Null);
    let id = RequestId::from("cross-file-exact-bound".to_string());
    server.send_request(id.clone(), "workspace/symbol", json!({"query": ""}));
    let response = server.response(&id);
    assert!(
        response.error.is_none(),
        "exact cross-file bound failed: {response:?}"
    );
    let result = response.result.expect("exact cross-file bound result");
    let symbols = result.as_array().expect("workspace symbol array");
    assert_eq!(symbols.len(), 10_000);
    assert_eq!(
        symbols
            .iter()
            .filter(|symbol| symbol["name"] == "First" || symbol["name"] == "Second")
            .count(),
        2,
        "the two unit entries must be included in the shared count"
    );
    server.shutdown();
}

#[test]
fn workspace_symbols_reject_the_first_cross_file_result_over_the_bound() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("workspace");
    write_file(
        &root.join("First.pas"),
        &workspace_symbol_source("First", 4_999),
    );
    write_file(
        &root.join("Second.pas"),
        &workspace_symbol_source("Second", 5_000),
    );

    let mut server = TestServer::launch();
    server.initialize(&root, Value::Null);
    let id = RequestId::from("cross-file-over-bound".to_string());
    server.send_request(id.clone(), "workspace/symbol", json!({"query": ""}));
    let response = server.response(&id);
    let error = response
        .error
        .expect("the first cross-file result over the bound must fail");
    assert!(error.message.contains("10000"));
    assert!(error.message.contains("narrow"));
    server.shutdown();
}

#[test]
fn workspace_symbols_return_empty_for_an_empty_workspace() {
    let temp = tempfile::tempdir().unwrap();
    let mut server = TestServer::launch();
    server.initialize(temp.path(), Value::Null);

    let id = RequestId::from("empty-workspace-symbols".to_string());
    server.send_request(id.clone(), "workspace/symbol", json!({"query": "anything"}));
    let response = server.response(&id);
    assert!(response.error.is_none(), "{:?}", response.error);
    assert!(response.result.unwrap().as_array().unwrap().is_empty());
    server.shutdown();
}

#[test]
fn workspace_symbol_cancellation_handles_thousands_of_declarations() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("workspace");
    let source_path = root.join("Many.pas");
    let mut source = String::from("unit Many;\ninterface\nvar\n");
    for index in 0..4_000 {
        source.push_str(&format!("  Symbol{index}: Integer;\n"));
    }
    source.push_str("implementation\nend.\n");
    write_file(&source_path, &source);

    let mut server = TestServer::launch();
    server.initialize(&root, Value::Null);
    let id = RequestId::from("cancelled-workspace-symbols".to_string());
    server.send_request(id.clone(), "workspace/symbol", json!({"query": "Symbol"}));
    server.send_notification(
        "$/cancelRequest",
        json!({"id": "cancelled-workspace-symbols"}),
    );
    let response = server.response(&id);
    let error = response
        .error
        .expect("cancelled workspace symbol query must fail");
    assert_eq!(error.code, -32800);
    assert_eq!(error.message, "request cancelled");
    server.shutdown();
}

#[test]
fn document_symbol_cancellation_handles_thousands_of_declarations() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("workspace");
    let source_path = root.join("Many.pas");
    let mut source = String::from("unit Many;\ninterface\nvar\n");
    for index in 0..4_000 {
        source.push_str(&format!("  Symbol{index}: Integer;\n"));
    }
    source.push_str("implementation\nend.\n");
    write_file(&source_path, &source);

    let mut server = TestServer::launch();
    server.initialize(&root, Value::Null);
    let id = RequestId::from("cancelled-document-symbols".to_string());
    server.send_request(
        id.clone(),
        "textDocument/documentSymbol",
        json!({"textDocument": {"uri": uri(&source_path)}}),
    );
    server.send_notification(
        "$/cancelRequest",
        json!({"id": "cancelled-document-symbols"}),
    );
    let response = server.response(&id);
    let error = response
        .error
        .expect("cancelled document symbol query must fail");
    assert_eq!(error.code, -32800);
    assert_eq!(error.message, "request cancelled");
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
fn production_project_context_reports_configured_override_errors_as_warnings() {
    let environment = tempfile::tempdir().expect("isolated server environment");
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path();
    let source = root.join("Main.pas");
    let user_config = environment.path().join("config/delphi-tools/config.toml");
    write_file(&source, "unit Main; interface implementation end.\n");
    write_file(&user_config, "[properties\ninvalid = 'user'\n");

    let mut server = TestServer::launch_with_environment(environment);
    server.initialize(root, Value::Null);
    let id = RequestId::from("production-override-warning".to_string());
    server.send_request(
        id.clone(),
        "pascal/projectContext",
        json!({"textDocument": {"uri": uri(&source)}}),
    );
    let response = server.response(&id);

    assert!(response.error.is_none(), "{response:?}");
    let result = response.result.unwrap();
    let warnings = result["warnings"]
        .as_array()
        .expect("project context warnings");
    assert!(
        warnings.iter().any(|warning| warning
            .as_str()
            .is_some_and(|warning| { warning.contains(&user_config.display().to_string()) })),
        "configured override provenance was not returned: {warnings:?}"
    );
    server.shutdown();
}

#[test]
fn malformed_candidate_override_survives_incomplete_selection_and_blocks_navigation() {
    for (case_name, root_override) in [
        ("without-root-override", None),
        (
            "with-valid-root-override",
            Some("[properties]\nRoot = 'valid'\n"),
        ),
    ] {
        let temp = tempfile::tempdir().expect("temporary workspace");
        let root = temp.path();
        let app = root.join("app");
        let main = app.join("Main.pas");
        let provider = app.join("Provider.pas");
        let malformed = app.join(".delphi-tools.local.toml");
        let main_source = "unit Main;\ninterface\nuses Provider;\nprocedure Use;\nimplementation\nprocedure Use;\nbegin\n  ProviderRoutine;\nend;\nend.\n";
        write_file(&main, main_source);
        write_file(
            &provider,
            "unit Provider;\ninterface\nprocedure ProviderRoutine;\nimplementation\nprocedure ProviderRoutine;\nbegin\nend;\nend.\n",
        );
        for project_name in ["A", "B"] {
            write_file(
                &app.join(format!("{project_name}.dproj")),
                "<Project><PropertyGroup><MainSource>Main.pas</MainSource></PropertyGroup><ItemGroup><DCCReference Include=\"Provider.pas\" /></ItemGroup></Project>",
            );
        }
        write_file(&malformed, "[properties\ninvalid = 'candidate'\n");
        if let Some(root_override) = root_override {
            write_file(&root.join(".delphi-tools.local.toml"), root_override);
        }

        let mut server = TestServer::launch();
        server.initialize(root, Value::Null);

        let context_id = RequestId::from(format!("{case_name}-context"));
        server.send_request(
            context_id.clone(),
            "pascal/projectContext",
            json!({"textDocument": {"uri": uri(&main)}}),
        );
        let context_response = server.response(&context_id);
        assert!(context_response.error.is_none(), "{context_response:?}");
        let context = context_response.result.expect("project context result");
        let warnings = context["warnings"]
            .as_array()
            .expect("project context warnings");
        assert!(
            warnings.iter().any(|warning| warning
                .as_str()
                .is_some_and(|warning| warning.contains(&malformed.display().to_string()))),
            "candidate override provenance was not returned for {case_name}: {warnings:?}"
        );
        assert_eq!(context["candidates"].as_array().unwrap().len(), 2);
        assert!(context["selectedProjectUri"].is_null());

        let navigation_id = RequestId::from(format!("{case_name}-navigation"));
        server.send_request(
            navigation_id.clone(),
            "textDocument/declaration",
            navigation_params(&main, main_source, "ProviderRoutine", 0),
        );
        assert!(
            result_locations(server.response(&navigation_id)).is_empty(),
            "navigation resolved through malformed candidate override for {case_name}"
        );
        server.shutdown();
    }
}

#[cfg(unix)]
#[test]
fn malformed_delphi_override_scopes_report_provenance_fail_closed_and_preserve_valid_scopes() {
    let main_source = "unit Main;\ninterface\nuses Provider;\nimplementation\nprocedure Run;\nbegin\n  ProviderRoutine;\nend;\nend.\n";
    let provider_source = "unit Provider;\ninterface\nprocedure ProviderRoutine;\nimplementation\nprocedure ProviderRoutine; begin end;\nend.\n";

    for scope in ["user", "workspace", "project"] {
        let environment = tempfile::tempdir().expect("isolated server environment");
        let root = tempfile::tempdir().expect("workspace root");
        let bad = root.path().join("bad");
        let good = root.path().join("good");
        let bad_project_dir = if scope == "workspace" {
            bad.join("project")
        } else {
            bad.clone()
        };
        let good_project_dir = if scope == "workspace" {
            good.join("project")
        } else {
            good.clone()
        };
        let bad_sdk = tempfile::tempdir().expect("bad SDK");
        let good_sdk = tempfile::tempdir().expect("good SDK");
        let bad_main = bad_project_dir.join("Main.pas");
        let good_main = good_project_dir.join("Main.pas");
        let user_config = environment.path().join("config/delphi-tools/config.toml");
        let root_override = root.path().join(".delphi-tools.local.toml");
        let bad_override = bad.join(".delphi-tools.local.toml");
        let good_override = good.join(".delphi-tools.local.toml");
        let bad_provider = bad_sdk.path().join("source/Provider.pas");
        let good_provider = good_sdk.path().join("source/Provider.pas");
        let malformed = match scope {
            "user" => user_config.clone(),
            "workspace" | "project" => bad_override.clone(),
            _ => unreachable!("known malformed override scope"),
        };
        let mapping = |prefix: &str, sdk: &Path| {
            format!(
                "[[path_mappings]]\nfrom = '{prefix}'\nto = '{}'\n",
                sdk.display()
            )
        };

        write_file(&bad_main, main_source);
        write_file(&good_main, main_source);
        write_file(&bad_provider, provider_source);
        write_file(&good_provider, provider_source);
        for (directory, provider) in [
            (&bad_project_dir, &bad_provider),
            (&good_project_dir, &good_provider),
        ] {
            write_file(
                &directory.join("Main.dproj"),
                &format!(
                    "<Project><PropertyGroup><MainSource>Main.pas</MainSource><DCC_UnitSearchPath>C:\\SDK\\source</DCC_UnitSearchPath></PropertyGroup><ItemGroup><DCCReference Include=\"{}\" /></ItemGroup></Project>",
                    provider.display()
                ),
            );
        }
        write_file(&user_config, &mapping("C:\\SDK", bad_sdk.path()));
        match scope {
            "user" => {
                write_file(&bad_override, &mapping("C:\\SDK", bad_sdk.path()));
            }
            "workspace" => {
                write_file(&bad_override, &mapping("C:\\SDK", bad_sdk.path()));
                write_file(&good_override, &mapping("C:\\SDK", good_sdk.path()));
            }
            "project" => {
                write_file(&root_override, &mapping("C:\\SDK", bad_sdk.path()));
                write_file(&bad_override, &mapping("C:\\SDK", bad_sdk.path()));
                write_file(&good_override, &mapping("C:\\SDK", good_sdk.path()));
            }
            _ => unreachable!("known valid override scope"),
        }

        {
            let mut server = TestServer::launch_with_environment_path(environment.path());
            if scope == "workspace" {
                server.initialize_with_workspace_folders(
                    root.path(),
                    &[bad.as_path(), good.as_path()],
                    Value::Null,
                );
            } else {
                server.initialize(root.path(), Value::Null);
            }
            let control_id = RequestId::from(format!("{scope}-valid-control"));
            server.send_request(
                control_id.clone(),
                "textDocument/definition",
                navigation_params(&bad_main, main_source, "ProviderRoutine", 0),
            );
            let control_locations = result_locations(server.response(&control_id));
            assert_eq!(
                control_locations.len(),
                1,
                "valid {scope} control did not navigate"
            );
            assert_eq!(control_locations[0]["uri"], uri(&bad_provider).to_string());
            server.shutdown();
        }

        write_file(&malformed, "[properties]\nBad-Key = 'invalid property'\n");

        let mut server = TestServer::launch_with_environment_path(environment.path());
        if scope == "workspace" {
            server.initialize_with_workspace_folders(
                root.path(),
                &[bad.as_path(), good.as_path()],
                Value::Null,
            );
        } else {
            server.initialize(root.path(), Value::Null);
        }

        let bad_context_id = RequestId::from(format!("{scope}-malformed-context"));
        server.send_request(
            bad_context_id.clone(),
            "pascal/projectContext",
            json!({"textDocument": {"uri": uri(&bad_main)}}),
        );
        let bad_context_response = server.response(&bad_context_id);
        assert!(
            bad_context_response.error.is_none(),
            "{bad_context_response:?}"
        );
        let bad_context = bad_context_response.result.unwrap();
        let warnings = bad_context["warnings"]
            .as_array()
            .expect("malformed project context warnings");
        assert!(
            warnings.iter().any(|warning| {
                warning.as_str().is_some_and(|warning| {
                    warning.contains(&malformed.display().to_string())
                        && warning.contains("Bad-Key")
                })
            }),
            "malformed {scope} provenance was not returned: {warnings:?}"
        );

        let bad_navigation_id = RequestId::from(format!("{scope}-malformed-navigation"));
        server.send_request(
            bad_navigation_id.clone(),
            "textDocument/definition",
            navigation_params(&bad_main, main_source, "ProviderRoutine", 0),
        );
        assert!(
            result_locations(server.response(&bad_navigation_id)).is_empty(),
            "navigation used malformed {scope} overrides"
        );

        if scope != "user" {
            let good_context_id = RequestId::from(format!("{scope}-valid-context"));
            server.send_request(
                good_context_id.clone(),
                "pascal/projectContext",
                json!({"textDocument": {"uri": uri(&good_main)}}),
            );
            let good_context_response = server.response(&good_context_id);
            assert!(
                good_context_response.error.is_none(),
                "{good_context_response:?}"
            );
            let good_context = good_context_response.result.unwrap();
            let good_warnings = good_context["warnings"]
                .as_array()
                .expect("valid project context warnings");
            assert!(
                good_warnings.iter().all(|warning| {
                    !warning
                        .as_str()
                        .unwrap_or_default()
                        .contains(&malformed.display().to_string())
                }),
                "malformed {scope} warning leaked into unrelated valid scope: {good_warnings:?}"
            );

            let good_navigation_id = RequestId::from(format!("{scope}-valid-navigation"));
            server.send_request(
                good_navigation_id.clone(),
                "textDocument/definition",
                navigation_params(&good_main, main_source, "ProviderRoutine", 0),
            );
            let good_locations = result_locations(server.response(&good_navigation_id));
            assert_eq!(
                good_locations.len(),
                1,
                "valid {scope} scope did not navigate"
            );
            assert_eq!(good_locations[0]["uri"], uri(&good_provider).to_string());
        }

        server.shutdown();
    }
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
fn type_definition_requests_follow_the_selected_project_unit_binding() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let main = root.join("Main.pas");
    let a_unit = root.join("a/Shared.pas");
    let b_unit = root.join("b/Shared.pas");
    let main_source = "unit Main;\ninterface\nuses Shared;\nimplementation\nprocedure Run;\nvar Item: TShared;\nbegin\n  Item := nil;\nend;\nend.\n";
    let unit_source =
        "unit Shared;\ninterface\ntype\n  TShared = class end;\nimplementation\nend.\n";
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
        ("type-definition-select-a", root.join("A.dproj"), a_unit),
        ("type-definition-select-b", root.join("B.dproj"), b_unit),
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

        let type_definition_id = RequestId::from(format!("{id}-request"));
        server.send_request(
            type_definition_id.clone(),
            "textDocument/typeDefinition",
            navigation_params(&main, main_source, "Item: TShared", 0),
        );
        let locations = result_locations(server.response(&type_definition_id));
        assert_eq!(
            locations.len(),
            1,
            "{id} should resolve one type declaration"
        );
        assert_eq!(locations[0]["uri"], uri(&expected).to_string());
        assert_eq!(
            locations[0]["range"],
            json!({
                "start": {"line": 3, "character": 2},
                "end": {"line": 3, "character": 9}
            })
        );
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
fn rejected_open_required_provider_never_falls_back_to_disk_for_highlights() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path().join("workspace");
    let main = root.join("Main.pas");
    let provider = root.join("Provider.pas");
    let main_source = "unit Main;\ninterface\nuses Provider;\nimplementation\nprocedure Run;\nbegin\n  Log(DiskValue);\nend;\nend.\n";
    let provider_source = "unit Provider;\ninterface\nconst DiskValue = 1;\nimplementation\nend.\n";
    write_file(&main, main_source);
    write_file(&provider, provider_source);

    let mut server = TestServer::launch();
    server.initialize(&root, json!({"maxFileBytes": 256}));
    server.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri(&provider),
                "languageId": "pascal",
                "version": 1,
                "text": provider_source
            }
        }),
    );
    let _ = server.notification("textDocument/publishDiagnostics");

    let oversized_overlay = format!("{provider_source}{}", "x".repeat(400));
    server.send_notification(
        "textDocument/didChange",
        json!({
            "textDocument": {"uri": uri(&provider), "version": 2},
            "contentChanges": [{"text": oversized_overlay}]
        }),
    );
    let rejection = server.notification("textDocument/publishDiagnostics");
    assert!(
        rejection["diagnostics"][0]["message"]
            .as_str()
            .expect("rejection diagnostic")
            .contains("per-file limit")
    );

    let request_id = RequestId::from("rejected-required-provider-highlights".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/documentHighlight",
        json!({
            "textDocument": {"uri": uri(&main)},
            "position": position_of(main_source, "DiskValue", 0)
        }),
    );
    let response = server.response(&request_id);
    let error = response
        .error
        .expect("required rejected provider must fail closed");
    assert!(
        error.message.to_ascii_lowercase().contains("rejected"),
        "unexpected required-provider error: {error:?}"
    );
    assert!(response.result.is_none());
    server.shutdown();
}

#[test]
fn unrelated_rejected_open_buffer_does_not_block_local_highlights() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path().join("workspace");
    let main = root.join("Main.pas");
    let unrelated = root.join("Unrelated.pas");
    let main_source = "unit Main;\ninterface\nconst LocalValue = 1;\nimplementation\nprocedure Run;\nbegin\n  Log(LocalValue);\nend;\nend.\n";
    let unrelated_source = "unit Unrelated;\ninterface\nconst Noise = 1;\nimplementation\nend.\n";
    write_file(&main, main_source);
    write_file(&unrelated, unrelated_source);

    let mut server = TestServer::launch();
    server.initialize(&root, json!({"maxFileBytes": 256}));
    server.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri(&unrelated),
                "languageId": "pascal",
                "version": 1,
                "text": format!("{unrelated_source}{}", "x".repeat(400))
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

    let request_id = RequestId::from("unrelated-rejected-local-highlights".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/documentHighlight",
        json!({
            "textDocument": {"uri": uri(&main)},
            "position": position_of(main_source, "LocalValue", 0)
        }),
    );
    let response = server.response(&request_id);
    assert!(response.error.is_none(), "{response:?}");
    assert_eq!(response.result.unwrap().as_array().unwrap().len(), 2);
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
fn native_unit_search_path_preserves_legacy_external_discovery_without_overrides() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let project_root = temp.path().join("project");
    let main = project_root.join("Main.pas");
    let external = temp.path().join("external/ExternalUnit.pas");
    let main_source = "unit Main;\ninterface\nuses ExternalUnit;\nimplementation\nprocedure Run;\nbegin\n  ExternalRoutine;\nend;\nend.\n";
    let external_source = "unit ExternalUnit;\ninterface\nprocedure ExternalRoutine;\nimplementation\nprocedure ExternalRoutine; begin end;\nend.\n";
    write_file(&main, main_source);
    write_file(&external, external_source);
    write_file(
        &project_root.join("App.dproj"),
        &format!(
            "<Project><PropertyGroup><MainSource>Main.pas</MainSource><DCC_UnitSearchPath>{}</DCC_UnitSearchPath></PropertyGroup></Project>",
            external.parent().expect("external directory").display()
        ),
    );

    let mut server = TestServer::launch();
    server.initialize(&project_root, Value::Null);
    let request_id = RequestId::from("external-search-path".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/declaration",
        navigation_params(&main, main_source, "ExternalRoutine", 0),
    );
    let response = server.response(&request_id);
    assert!(
        response.error.is_none(),
        "unexpected protocol error: {response:?}"
    );
    let locations = result_locations(response);
    assert_eq!(locations.len(), 1);
    assert_eq!(locations[0]["uri"], uri(&external).to_string());
    server.shutdown();
}

#[test]
fn standalone_document_directory_remains_a_legacy_native_root_without_workspace() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let main = temp.path().join("Main.pas");
    let provider = temp.path().join("Provider.pas");
    let main_source = "unit Main;\ninterface\nuses Provider;\nimplementation\nprocedure Run;\nbegin\n  ProviderRoutine;\nend;\nend.\n";
    let provider_source = "unit Provider;\ninterface\nprocedure ProviderRoutine;\nimplementation\nprocedure ProviderRoutine; begin end;\nend.\n";
    write_file(&main, main_source);
    write_file(&provider, provider_source);

    let mut workspace = test_workspace(Vec::new(), WorkspaceOptions::default());
    let locations = workspace.navigate(
        &uri(&main),
        position_of(main_source, "ProviderRoutine", 0),
        NavigationTarget::Declaration,
    );

    assert_eq!(locations.len(), 1);
    assert_eq!(locations[0].uri, uri(&provider));
}

#[cfg(unix)]
#[test]
fn unrelated_overrides_do_not_revoke_a_literal_native_search_path() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let project_root = temp.path().join("project");
    let main = project_root.join("Main.pas");
    let external = temp.path().join("external/ExternalUnit.pas");
    let main_source = "unit Main;\ninterface\nuses ExternalUnit;\nimplementation\nprocedure Run;\nbegin\n  ExternalRoutine;\nend;\nend.\n";
    let external_source = "unit ExternalUnit;\ninterface\nprocedure ExternalRoutine;\nimplementation\nprocedure ExternalRoutine; begin end;\nend.\n";
    write_file(&main, main_source);
    write_file(&external, external_source);
    write_file(
        &project_root.join("App.dproj"),
        &format!(
            "<Project><PropertyGroup><MainSource>Main.pas</MainSource><DCC_UnitSearchPath>{}</DCC_UnitSearchPath></PropertyGroup></Project>",
            external.parent().expect("external directory").display()
        ),
    );
    write_file(
        &project_root.join(".delphi-tools.local.toml"),
        &format!(
            "[properties]\nUnrelated = 'value'\n[[path_mappings]]\nfrom = 'C:\\UNRELATED'\nto = '{}'\n",
            temp.path().join("unrelated-destination").display()
        ),
    );

    let mut workspace = test_workspace(vec![project_root], WorkspaceOptions::default());
    let locations = workspace.navigate(
        &uri(&main),
        position_of(main_source, "ExternalRoutine", 0),
        NavigationTarget::Declaration,
    );

    assert_eq!(locations.len(), 1);
    assert_eq!(locations[0].uri, uri(&external));
}

#[cfg(unix)]
#[test]
fn configured_property_search_paths_do_not_authorize_external_discovery() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let project_root = temp.path().join("project");
    let main = project_root.join("Main.pas");
    let external = temp.path().join("external/ExternalUnit.pas");
    let main_source = "unit Main;\ninterface\nuses ExternalUnit;\nimplementation\nprocedure Run;\nbegin\n  ExternalRoutine;\nend;\nend.\n";
    let external_source = "unit ExternalUnit;\ninterface\nprocedure ExternalRoutine;\nimplementation\nprocedure ExternalRoutine; begin end;\nend.\n";
    write_file(&main, main_source);
    write_file(&external, external_source);
    write_file(
        &project_root.join("App.dproj"),
        "<Project><PropertyGroup><MainSource>Main.pas</MainSource></PropertyGroup></Project>",
    );
    write_file(
        &project_root.join(".delphi-tools.local.toml"),
        &format!(
            "[properties]\nDCC_UnitSearchPath = '{}'\n",
            external.parent().expect("external directory").display()
        ),
    );

    let mut workspace = test_workspace(vec![project_root], WorkspaceOptions::default());
    let locations = workspace.navigate(
        &uri(&main),
        position_of(main_source, "ExternalRoutine", 0),
        NavigationTarget::Declaration,
    );
    assert!(
        locations.is_empty(),
        "override-configured search paths must not authorize external discovery"
    );
    assert_eq!(
        workspace.parsed_document_count(),
        1,
        "unauthorized configured sources must not be read or indexed"
    );
}

#[cfg(unix)]
#[test]
fn case_adjusted_mapped_explicit_references_reject_symlink_and_excluded_sources() {
    use std::os::unix::fs::symlink;

    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path().join("workspace");
    let main = root.join("Main.pas");
    let project = root.join("App.dproj");
    let sdk = temp.path().join("SDK");
    let mapped_sdk = temp.path().join("sdk");
    let outside = temp.path().join("outside");
    let main_source = "unit Main;\ninterface\nuses Provider, HiddenProvider;\nimplementation\nprocedure Run;\nbegin\n  ProviderRoutine;\n  HiddenRoutine;\nend;\nend.\n";
    let provider_source = "unit Provider;\ninterface\nprocedure ProviderRoutine;\nimplementation\nprocedure ProviderRoutine; begin end;\nend.\n";
    let hidden_source = "unit HiddenProvider;\ninterface\nprocedure HiddenRoutine;\nimplementation\nprocedure HiddenRoutine; begin end;\nend.\n";
    write_file(&main, main_source);
    write_file(
        &project,
        "<Project><PropertyGroup><MainSource>Main.pas</MainSource></PropertyGroup><ItemGroup><DCCReference Include=\"C:\\SDK\\escape\\Provider.pas\" /><DCCReference Include=\"C:\\SDK\\.git\\HiddenProvider.pas\" /></ItemGroup></Project>",
    );
    write_file(&outside.join("Provider.pas"), provider_source);
    write_file(&sdk.join(".git/HiddenProvider.pas"), hidden_source);
    symlink(&outside, sdk.join("escape")).expect("mapped escape symlink");
    write_file(
        &root.join(".delphi-tools.local.toml"),
        &format!(
            "[[path_mappings]]\nfrom = 'C:\\SDK'\nto = '{}'\n",
            mapped_sdk.display()
        ),
    );

    let mut workspace = test_workspace(vec![root], WorkspaceOptions::default());
    let main_uri = uri(&main);
    let provider_locations = workspace.navigate(
        &main_uri,
        position_of(main_source, "ProviderRoutine", 0),
        NavigationTarget::Declaration,
    );
    assert!(
        provider_locations.is_empty(),
        "symlinked mapped explicit references must be rejected"
    );
    assert_eq!(
        workspace.parsed_document_count(),
        1,
        "rejected mapped sources must not be read or indexed"
    );

    let hidden_locations = workspace.navigate(
        &main_uri,
        position_of(main_source, "HiddenRoutine", 0),
        NavigationTarget::Declaration,
    );
    assert!(
        hidden_locations.is_empty(),
        "excluded mapped explicit references must be rejected"
    );
    assert_eq!(
        workspace.parsed_document_count(),
        1,
        "excluded mapped sources must not be read or indexed"
    );
}

#[cfg(unix)]
#[test]
fn case_adjusted_mapped_main_source_is_not_authorized_through_membership() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let project_root = temp.path().join("project");
    let project = project_root.join("App.dproj");
    let sdk = temp.path().join("SDK");
    let mapped_sdk = temp.path().join("sdk");
    let outside = temp.path().join("outside");
    let main = sdk.join("escape/Main.pas");
    let main_source =
        "program App;\nprocedure MainRoutine;\nbegin\nend;\nbegin\n  MainRoutine;\nend.\n";

    fs::create_dir_all(&sdk).expect("mapped SDK directory");
    fs::create_dir_all(&outside).expect("outside directory");
    write_file(&outside.join("Main.pas"), main_source);
    write_file(
        &project,
        "<Project><PropertyGroup><MainSource>C:\\SDK\\escape\\Main.pas</MainSource></PropertyGroup></Project>",
    );
    write_file(
        &project_root.join(".delphi-tools.local.toml"),
        &format!(
            "[[path_mappings]]\nfrom = 'C:\\SDK'\nto = '{}'\n",
            mapped_sdk.display()
        ),
    );
    symlink(&outside, sdk.join("escape")).expect("mapped escape symlink");

    let mut workspace = test_workspace(
        vec![project_root],
        WorkspaceOptions {
            project_file: Some(project),
            ..WorkspaceOptions::default()
        },
    );
    let locations = workspace.navigate(
        &uri(&main),
        position_of(main_source, "MainRoutine", 0),
        NavigationTarget::Declaration,
    );

    assert!(
        locations.is_empty(),
        "case-adjusted mapped MainSource must be rejected"
    );
    assert_eq!(
        workspace.parsed_document_count(),
        0,
        "rejected mapped MainSource must not be read or indexed"
    );
}

#[cfg(unix)]
#[test]
fn configured_native_main_and_explicit_paths_use_the_effective_mapped_root() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let project_root = temp.path().join("project");
    let project = project_root.join("App.dproj");
    let sdk = temp.path().join("sdk");
    let main = sdk.join("Main.pas");
    let provider = sdk.join("Provider.pas");
    let main_source = "unit Main;\ninterface\nuses Provider;\nimplementation\nprocedure Run;\nbegin\n  ProviderRoutine;\nend;\nend.\n";
    let provider_source = "unit Provider;\ninterface\nprocedure ProviderRoutine;\nimplementation\nprocedure ProviderRoutine; begin end;\nend.\n";
    write_file(&main, main_source);
    write_file(&provider, provider_source);
    write_file(
        &project,
        "<Project><PropertyGroup><MainSource>$(BDS)/Main.pas</MainSource></PropertyGroup><ItemGroup><DCCReference Include=\"$(BDS)/Provider.pas\" /></ItemGroup></Project>",
    );
    write_file(
        &project_root.join(".delphi-tools.local.toml"),
        &format!(
            "[properties]\nBDS = '{}'\n[[path_mappings]]\nfrom = 'C:\\SDK'\nto = '{}'\n",
            sdk.display(),
            sdk.display()
        ),
    );

    let mut workspace = test_workspace(
        vec![project_root],
        WorkspaceOptions {
            project_file: Some(project),
            ..WorkspaceOptions::default()
        },
    );
    let locations = workspace.navigate(
        &uri(&main),
        position_of(main_source, "ProviderRoutine", 0),
        NavigationTarget::Declaration,
    );

    assert_eq!(locations.len(), 1);
    assert_eq!(locations[0].uri, uri(&provider));
}

#[cfg(unix)]
#[test]
fn native_bds_mapped_dpr_membership_resolves_a_filename_mismatch_without_dcc_reference() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let project_root = temp.path().join("project");
    let project = project_root.join("App.dproj");
    let sdk = temp.path().join("sdk");
    let main = sdk.join("Launcher.dpr");
    let provider = sdk.join("ProviderSource.pas");
    let main_source = "program Launcher;\nuses Provider in 'ProviderSource.pas';\nprocedure Run;\nbegin\n  ProviderRoutine;\nend;\nbegin\n  Run;\nend.\n";
    let provider_source = "unit Provider;\ninterface\nprocedure ProviderRoutine;\nimplementation\nprocedure ProviderRoutine; begin end;\nend.\n";

    write_file(&main, main_source);
    write_file(&provider, provider_source);
    write_file(
        &project,
        "<Project><PropertyGroup><MainSource>$(BDS)\\Launcher.dpr</MainSource><BDS>C:\\SDK</BDS></PropertyGroup></Project>",
    );
    write_file(
        &project_root.join(".delphi-tools.local.toml"),
        &format!(
            "[properties]\nBDS = 'C:\\SDK'\n[[path_mappings]]\nfrom = 'C:\\SDK'\nto = '{}'\n",
            sdk.display()
        ),
    );

    let mut workspace = test_workspace(
        vec![project_root],
        WorkspaceOptions {
            project_file: Some(project),
            ..WorkspaceOptions::default()
        },
    );
    let locations = workspace.navigate(
        &uri(&main),
        position_of(main_source, "ProviderRoutine", 0),
        NavigationTarget::Declaration,
    );

    assert_eq!(locations.len(), 1);
    assert_eq!(locations[0].uri, uri(&provider));
}

#[cfg(unix)]
#[test]
fn configured_native_mapped_references_reject_symlink_and_excluded_paths() {
    use std::os::unix::fs::symlink;

    let temp = tempfile::tempdir().expect("temporary workspace");
    let project_root = temp.path().join("project");
    let main = project_root.join("Main.pas");
    let project = project_root.join("App.dproj");
    let sdk = temp.path().join("sdk");
    let outside = temp.path().join("outside");
    let main_source = "unit Main;\ninterface\nuses Provider, HiddenProvider;\nimplementation\nprocedure Run;\nbegin\n  ProviderRoutine;\n  HiddenRoutine;\nend;\nend.\n";
    let provider_source = "unit Provider;\ninterface\nprocedure ProviderRoutine;\nimplementation\nprocedure ProviderRoutine; begin end;\nend.\n";
    let hidden_source = "unit HiddenProvider;\ninterface\nprocedure HiddenRoutine;\nimplementation\nprocedure HiddenRoutine; begin end;\nend.\n";
    write_file(&main, main_source);
    write_file(
        &project,
        "<Project><PropertyGroup><MainSource>Main.pas</MainSource></PropertyGroup><ItemGroup><DCCReference Include=\"$(BDS)/escape/Provider.pas\" /><DCCReference Include=\"$(BDS)/.git/HiddenProvider.pas\" /></ItemGroup></Project>",
    );
    write_file(&outside.join("Provider.pas"), provider_source);
    write_file(&sdk.join(".git/HiddenProvider.pas"), hidden_source);
    symlink(&outside, sdk.join("escape")).expect("mapped escape symlink");
    write_file(
        &project_root.join(".delphi-tools.local.toml"),
        &format!(
            "[properties]\nBDS = '{}'\n[[path_mappings]]\nfrom = 'C:\\SDK'\nto = '{}'\n",
            sdk.display(),
            sdk.display()
        ),
    );

    let mut workspace = test_workspace(vec![project_root], WorkspaceOptions::default());
    let main_uri = uri(&main);
    let provider_locations = workspace.navigate(
        &main_uri,
        position_of(main_source, "ProviderRoutine", 0),
        NavigationTarget::Declaration,
    );
    assert!(
        provider_locations.is_empty(),
        "configured mapped symlink references must be rejected"
    );
    assert_eq!(workspace.parsed_document_count(), 1);

    let hidden_locations = workspace.navigate(
        &main_uri,
        position_of(main_source, "HiddenRoutine", 0),
        NavigationTarget::Declaration,
    );
    assert!(
        hidden_locations.is_empty(),
        "configured mapped excluded references must be rejected"
    );
    assert_eq!(workspace.parsed_document_count(), 1);
}

#[cfg(unix)]
#[test]
fn client_config_selection_keeps_native_search_path_legacy_without_override() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let project_root = temp.path().join("project");
    let main = project_root.join("Main.pas");
    let external = temp.path().join("external/Debug");
    let provider = external.join("Provider.pas");
    let main_source = "unit Main;\ninterface\nuses Provider;\nimplementation\nprocedure Run;\nbegin\n  ProviderRoutine;\nend;\nend.\n";
    let provider_source = "unit Provider;\ninterface\nprocedure ProviderRoutine;\nimplementation\nprocedure ProviderRoutine; begin end;\nend.\n";
    write_file(&main, main_source);
    write_file(&provider, provider_source);
    write_file(
        &project_root.join("App.dproj"),
        &format!(
            "<Project><PropertyGroup><MainSource>Main.pas</MainSource><DCC_UnitSearchPath>{}/$(Config)</DCC_UnitSearchPath></PropertyGroup></Project>",
            temp.path().join("external").display()
        ),
    );

    let mut workspace = test_workspace(
        vec![project_root],
        WorkspaceOptions {
            build_config: Some("Debug".to_owned()),
            ..WorkspaceOptions::default()
        },
    );
    let locations = workspace.navigate(
        &uri(&main),
        position_of(main_source, "ProviderRoutine", 0),
        NavigationTarget::Declaration,
    );

    assert_eq!(locations.len(), 1);
    assert_eq!(locations[0].uri, uri(&provider));
}

#[cfg(unix)]
#[test]
fn client_platform_selection_keeps_native_search_path_legacy_without_override() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let project_root = temp.path().join("project");
    let main = project_root.join("Main.pas");
    let external = temp.path().join("external/Win32");
    let provider = external.join("Provider.pas");
    let main_source = "unit Main;\ninterface\nuses Provider;\nimplementation\nprocedure Run;\nbegin\n  ProviderRoutine;\nend;\nend.\n";
    let provider_source = "unit Provider;\ninterface\nprocedure ProviderRoutine;\nimplementation\nprocedure ProviderRoutine; begin end;\nend.\n";
    write_file(&main, main_source);
    write_file(&provider, provider_source);
    write_file(
        &project_root.join("App.dproj"),
        &format!(
            "<Project><PropertyGroup><MainSource>Main.pas</MainSource><DCC_UnitSearchPath>{}/$(Platform)</DCC_UnitSearchPath></PropertyGroup></Project>",
            temp.path().join("external").display()
        ),
    );

    let mut workspace = test_workspace(
        vec![project_root],
        WorkspaceOptions {
            platform: Some("Win32".to_owned()),
            ..WorkspaceOptions::default()
        },
    );
    let locations = workspace.navigate(
        &uri(&main),
        position_of(main_source, "ProviderRoutine", 0),
        NavigationTarget::Declaration,
    );

    assert_eq!(locations.len(), 1);
    assert_eq!(locations[0].uri, uri(&provider));
}

#[cfg(unix)]
#[test]
fn client_selection_replaces_configured_property_for_source_authorization() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let project_root = temp.path().join("project");
    let main = project_root.join("Main.pas");
    let external = temp.path().join("external/Debug");
    let provider = external.join("Provider.pas");
    let main_source = "unit Main;\ninterface\nuses Provider;\nimplementation\nprocedure Run;\nbegin\n  ProviderRoutine;\nend;\nend.\n";
    let provider_source = "unit Provider;\ninterface\nprocedure ProviderRoutine;\nimplementation\nprocedure ProviderRoutine; begin end;\nend.\n";
    write_file(&main, main_source);
    write_file(&provider, provider_source);
    write_file(
        &project_root.join(".delphi-tools.local.toml"),
        "[properties]\nConfig = 'FileValue'\n",
    );
    write_file(
        &project_root.join("App.dproj"),
        &format!(
            "<Project><PropertyGroup><MainSource>Main.pas</MainSource><DCC_UnitSearchPath>{}/$(Config)</DCC_UnitSearchPath></PropertyGroup></Project>",
            temp.path().join("external").display()
        ),
    );

    let mut workspace = test_workspace(
        vec![project_root],
        WorkspaceOptions {
            build_config: Some("Debug".to_owned()),
            ..WorkspaceOptions::default()
        },
    );
    let locations = workspace.navigate(
        &uri(&main),
        position_of(main_source, "ProviderRoutine", 0),
        NavigationTarget::Declaration,
    );

    assert_eq!(locations.len(), 1);
    assert_eq!(locations[0].uri, uri(&provider));
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
    let mut workspace = test_workspace(vec![root], WorkspaceOptions::default());
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

    let mut workspace = test_workspace(
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

    let session = OverrideSession::new(None);
    let context = ProjectContext::discover_with_overrides(
        &main,
        std::slice::from_ref(&root),
        &pascal_lsp::ProjectOptions::default(),
        &session,
    )
    .expect("discover project context");
    assert!(context.warnings.is_empty());

    let mut workspace = test_workspace(vec![root], WorkspaceOptions::default());
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

    let mut workspace = test_workspace(vec![root], WorkspaceOptions::default());
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

    let mut workspace = test_workspace(vec![root], WorkspaceOptions::default());
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
    let mut workspace = test_workspace(vec![root], WorkspaceOptions::default());
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

#[cfg(unix)]
#[test]
fn package_metadata_inherits_requesting_project_overrides_without_package_local_leakage() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path().join("workspace");
    let package_dir = root.join("packages");
    let sdk_a = temp.path().join("sdk-a");
    let sdk_b = temp.path().join("sdk-b");
    let sdk_wrong = temp.path().join("sdk-wrong");
    let main_source = "unit Main;\ninterface\nuses SharedUnit;\nimplementation\nprocedure Run;\nbegin\n  SharedRoutine;\nend;\nend.\n";
    let package_project = package_dir.join("Shared.dproj");
    let package_metadata = "<Project><PropertyGroup><MainSource>SharedMain.dpk</MainSource></PropertyGroup><Import Project=\"C:\\SDK\\Package.optset\" /></Project>";
    let provider_source = |name: &str| {
        format!(
            "unit SharedUnit;\ninterface\nprocedure SharedRoutine;\nimplementation\nprocedure SharedRoutine; begin end;\nend.\n// {name}\n",
            name = name
        )
    };

    for (project_name, sdk, provider_name, config, platform, branch) in [
        ("A", &sdk_a, "A", "Debug", "Win32", "debug"),
        ("B", &sdk_b, "B", "Release", "Win64", "release"),
    ] {
        let main = root.join(project_name).join("Main.pas");
        write_file(&main, main_source);
        write_file(
            &root.join(project_name).join("App.dproj"),
            &format!(
                "<Project><PropertyGroup><MainSource>Main.pas</MainSource><Config>{config}</Config><Platform>{platform}</Platform><DCC_UsePackage>Shared</DCC_UsePackage></PropertyGroup></Project>"
            ),
        );
        write_file(
            &sdk.join(format!("source/{branch}/SharedUnit.pas")),
            &provider_source(provider_name),
        );
        write_file(
            &sdk.join("Package.optset"),
            &format!(
                "<Project><ItemGroup><DCCReference Include=\"C:\\SDK\\source\\{branch}\\SharedUnit.pas\" /></ItemGroup></Project>"
            ),
        );
        write_file(
            &root.join(project_name).join(".delphi-tools.local.toml"),
            &format!(
                "[properties]\nBDS = 'C:\\SDK'\n[[path_mappings]]\nfrom = 'C:\\SDK'\nto = '{}'\n",
                sdk.display()
            ),
        );
    }
    write_file(&package_project, package_metadata);
    write_file(
        &sdk_wrong.join("source/debug/SharedUnit.pas"),
        &provider_source("wrong package-local mapping"),
    );
    write_file(
        &package_dir.join(".delphi-tools.local.toml"),
        &format!(
            "[properties]\nBDS = 'C:\\SDK'\n[[path_mappings]]\nfrom = 'C:\\SDK'\nto = '{}'\n",
            sdk_wrong.display()
        ),
    );

    let mut workspace = test_workspace(vec![root.clone()], WorkspaceOptions::default());
    for (sdk, expected, branch) in [
        (&sdk_a, "A", "debug"),
        (&sdk_b, "B", "release"),
        (&sdk_a, "A", "debug"),
    ] {
        let main = root.join(expected).join("Main.pas");
        let locations = workspace.navigate(
            &uri(&main),
            position_of(main_source, "SharedRoutine", 0),
            NavigationTarget::Declaration,
        );
        assert_eq!(
            locations.len(),
            1,
            "package lookup {expected}/{branch} should resolve"
        );
        assert_eq!(
            locations[0].uri,
            uri(&sdk.join(format!("source/{branch}/SharedUnit.pas")))
        );
        assert!(
            !locations[0]
                .uri
                .to_file_path()
                .unwrap()
                .starts_with(&sdk_wrong),
            "package-local overrides must not affect the requesting project"
        );
    }

    let project_override = root.join("A/.delphi-tools.local.toml");
    write_file(&project_override, "[properties]\nBDS = 'C:\\SDK'\n");
    let mut revoked_workspace = test_workspace(vec![root.clone()], WorkspaceOptions::default());
    let removed_grant = revoked_workspace.navigate(
        &uri(&root.join("A/Main.pas")),
        position_of(main_source, "SharedRoutine", 0),
        NavigationTarget::Declaration,
    );
    assert!(
        removed_grant.is_empty(),
        "removing the requesting project's mapping must revoke native package reads: {removed_grant:?}"
    );
}

#[test]
fn unrelated_missing_mapping_does_not_block_workspace_package_lookup() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path().join("workspace");
    let main = root.join("Main.pas");
    let package = root.join("packages/Shared.dpk");
    let provider = root.join("packages/SharedUnit.pas");
    let main_source = "unit Main;\ninterface\nuses SharedUnit;\nimplementation\nprocedure Run;\nbegin\n  SharedRoutine;\nend;\nend.\n";
    let provider_source = "unit SharedUnit;\ninterface\nprocedure SharedRoutine;\nimplementation\nprocedure SharedRoutine; begin end;\nend.\n";
    write_file(&main, main_source);
    write_file(&provider, provider_source);
    write_file(
        &package,
        "package Shared;\ncontains\n  SharedUnit in 'SharedUnit.pas';\nend.\n",
    );
    write_file(
        &root.join("App.dproj"),
        "<Project><PropertyGroup><MainSource>Main.pas</MainSource><DCC_UsePackage>Shared</DCC_UsePackage></PropertyGroup></Project>",
    );
    write_file(
        &root.join(".delphi-tools.local.toml"),
        &format!(
            "[[path_mappings]]\nfrom = 'C:\\MissingSdk'\nto = '{}'\n",
            temp.path().join("missing-sdk").display()
        ),
    );

    let mut workspace = test_workspace(vec![root], WorkspaceOptions::default());
    let locations = workspace.navigate(
        &uri(&main),
        position_of(main_source, "SharedRoutine", 0),
        NavigationTarget::Declaration,
    );

    assert_eq!(locations.len(), 1);
    assert_eq!(locations[0].uri, uri(&provider));
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
    let mut workspace = test_workspace(vec![root], WorkspaceOptions::default());
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

    let mut workspace = test_workspace(vec![root], WorkspaceOptions::default());
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

    let mut workspace = test_workspace(vec![root], WorkspaceOptions::default());
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

    let mut workspace = test_workspace(vec![root], WorkspaceOptions::default());
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
    let mut workspace = test_workspace(vec![root], WorkspaceOptions::default());
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

    let mut workspace = test_workspace(vec![root], WorkspaceOptions::default());
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

    let mut workspace = test_workspace(vec![root], WorkspaceOptions::default());
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

    let mut target_workspace = test_workspace(vec![root.clone()], WorkspaceOptions::default());
    let target_locations = target_workspace.navigate(
        &uri(&target_main),
        position_of(target_source, "TargetRoutine", 0),
        NavigationTarget::Declaration,
    );
    assert_eq!(target_locations.len(), 1);
    assert_eq!(target_locations[0].uri, uri(&target_provider));

    let mut duplicate_workspace = test_workspace(vec![root], WorkspaceOptions::default());
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
fn rename_allows_harmless_mapped_include_under_an_overlapping_legacy_root() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path().join("fixture");
    let main = root.join("Main.pas");
    let include = root.join("BuildDirectives.inc");
    let source = "unit Main;\ninterface\n{$I 'C:\\SDK\\BuildDirectives.inc'}\nconst\n  badConst = 1;\n{$IFDEF FEATURE}\nconst\n  unrelatedValue = 2;\n{$ENDIF}\nimplementation\nend.\n";
    write_file(&include, "{$DEFINE FEATURE}\n{$METHODINFO ON}\n");
    write_file(&main, source);
    write_file(
        &root.join(".delphi-tools.local.toml"),
        &format!(
            "[[path_mappings]]\nfrom = 'C:\\SDK'\nto = '{}'\n",
            root.display()
        ),
    );

    let mut server = TestServer::launch();
    server.initialize(&root, Value::Null);
    let request_id = RequestId::from("mapped-harmless-directives-rename".to_string());
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
        "harmless mapped include must not be rejected by the overlapping legacy root: {response:?}"
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

#[test]
fn rename_rejects_a_reference_in_an_unknown_boolean_comparison_branch() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path().join("fixture");
    let main = root.join("Main.pas");
    let source = "unit Main;\ninterface\nimplementation\nprocedure Run;\nvar\n  Value: Integer;\nbegin\n{$IF DEFINED(A) = False}\n  Value := 2;\n{$ENDIF}\n  Value := 1;\nend;\nend.\n";
    write_file(&main, source);

    let mut server = TestServer::launch();
    server.initialize(&root, Value::Null);
    let request_id = RequestId::from("unknown-comparison-rename".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/rename",
        json!({
            "textDocument": {"uri": uri(&main)},
            "position": position_of(source, "Value", 0),
            "newName": "RenamedValue"
        }),
    );
    let response = server.response(&request_id);
    let error = response
        .error
        .expect("unknown comparison branch must fail closed");
    assert!(
        error.message.to_ascii_lowercase().contains("conditional")
            || error.message.to_ascii_lowercase().contains("binding"),
        "unexpected unknown-comparison rename error: {}",
        error.message
    );
    server.shutdown();
}

#[test]
fn rename_rejects_an_include_after_the_owner_undefines_a_project_define() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path().join("fixture");
    let main = root.join("Main.pas");
    let include = root.join("Use.inc");
    let source = "unit Main;\ninterface\nimplementation\nprocedure Run;\nvar\n  Value: Integer;\nbegin\n{$UNDEF FEATURE}\n{$I Use.inc}\n  Value := 1;\nend;\nend.\n";
    write_file(&main, source);
    write_file(&include, "{$IFNDEF FEATURE}\n  Value := 2;\n{$ENDIF}\n");
    write_file(
        &root.join("App.dproj"),
        "<Project><PropertyGroup><MainSource>Main.pas</MainSource><DCC_Define>FEATURE</DCC_Define></PropertyGroup></Project>",
    );

    let mut server = TestServer::launch();
    server.initialize(&root, json!({"projectFile": "App.dproj"}));
    let request_id = RequestId::from("owner-undef-include-rename".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/rename",
        json!({
            "textDocument": {"uri": uri(&main)},
            "position": position_of(source, "Value", 0),
            "newName": "RenamedValue"
        }),
    );
    let response = server.response(&request_id);
    let error = response
        .error
        .expect("an include must use the owner\'s invalidated define state");
    assert_eq!(error.code, -32803);
    assert!(
        error.message.to_ascii_lowercase().contains("include"),
        "unexpected owner-undef include error: {error:?}"
    );
    server.shutdown();
}

#[test]
fn rename_rejects_a_pascal_identifier_in_a_source_conditional_expression() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path().join("fixture");
    let main = root.join("Main.pas");
    let source = "unit Main;\ninterface\nconst Flag = False;\n{$IF Flag}\nconst Other = 1;\n{$ENDIF}\nimplementation\nend.\n";
    write_file(&main, source);

    let mut server = TestServer::launch();
    server.initialize(&root, Value::Null);
    let request_id = RequestId::from("pascal-conditional-reference-rename".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/rename",
        json!({
            "textDocument": {"uri": uri(&main)},
            "position": position_of(source, "Flag", 0),
            "newName": "NewName"
        }),
    );
    let response = server.response(&request_id);
    let error = response
        .error
        .expect("a Pascal conditional reference must not be omitted from rename");
    assert_eq!(error.code, -32803);
    assert!(
        error.message.to_ascii_lowercase().contains("conditional")
            || error.message.to_ascii_lowercase().contains("incomplete"),
        "unexpected Pascal conditional rename error: {error:?}"
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

#[test]
fn recursive_generic_constraint_completion_fails_closed_and_keeps_server_responsive() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let source_path = temp.path().join("RecursiveGenericConstraint.pas");
    let source = "unit RecursiveGenericConstraint;\ninterface\ntype\n  TWrap<T> = class\n    Value: T;\n  end;\n  TNode<T: TNode<TWrap<T>>> = class\n    Value: T;\n  end;\n  TImpl = class(TNode<TImpl>)\n    Member: Integer;\n  end;\nimplementation\nprocedure Run;\nvar\n  Box: TNode<TImpl>;\nbegin\n  Box.Value.Member;\nend;\nend.\n";
    write_file(&source_path, source);

    let mut server = TestServer::launch();
    server.initialize(temp.path(), Value::Null);

    let completion_id = RequestId::from("recursive-generic-constraint-completion".to_string());
    server.send_request(
        completion_id.clone(),
        "textDocument/completion",
        json!({
            "textDocument": {"uri": uri(&source_path)},
            "position": position_after(source, "Box.Value.", 0),
        }),
    );
    let completion = server.response(&completion_id);
    assert!(
        completion.error.is_none(),
        "recursive generic completion failed: {completion:?}"
    );
    let result = completion
        .result
        .expect("recursive generic completion result");
    assert!(result["items"].is_array());
    assert_eq!(result["isIncomplete"], true);

    let responsive_id = RequestId::from("recursive-generic-constraint-responsive".to_string());
    server.send_request(
        responsive_id.clone(),
        "textDocument/documentSymbol",
        json!({"textDocument": {"uri": uri(&source_path)}}),
    );
    let responsive = server.response(&responsive_id);
    assert!(
        responsive.error.is_none(),
        "server stopped responding after recursive generic completion: {responsive:?}"
    );
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn blocked_navigation_does_not_block_unrelated_lsp_requests() {
    let environment = tempfile::tempdir().expect("isolated server environment");
    let root = environment.path().join("workspace");
    let provider = root.join("Provider.pas");
    let main = root.join("Main.pas");
    let provider_source = "unit Provider;\ninterface\nprocedure PublicRoutine;\nimplementation\nprocedure PublicRoutine;\nbegin\nend;\nend.\n";
    let main_source = "unit Main;\ninterface\nuses Provider;\nimplementation\nprocedure Run;\nbegin\n  PublicRoutine;\nend;\nend.\n";
    write_file(&provider, provider_source);
    write_file(&main, main_source);

    let (mut server, barrier) = TestServer::launch_with_navigation_barrier(environment);
    server.initialize(&root, Value::Null);

    let navigation_id = RequestId::from("blocked-navigation".to_string());
    server.send_request(
        navigation_id.clone(),
        "textDocument/definition",
        navigation_params(&main, main_source, "PublicRoutine", 0),
    );
    barrier.wait_until_entered();

    let symbols_id = RequestId::from("while-navigation-is-blocked".to_string());
    server.send_request(
        symbols_id.clone(),
        "textDocument/documentSymbol",
        json!({"textDocument": {"uri": uri(&main)}}),
    );
    let symbols = server.response(&symbols_id);
    assert!(
        symbols.error.is_none(),
        "unrelated request was blocked by navigation: {symbols:?}"
    );
    assert!(
        symbols.result.is_some(),
        "document symbols must be returned"
    );

    barrier.release();
    let locations = result_locations(server.response(&navigation_id));
    assert_eq!(locations.len(), 1);
    assert_eq!(locations[0]["uri"], uri(&provider).to_string());
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn unrelated_open_document_change_does_not_discard_blocked_navigation_result() {
    let environment = tempfile::tempdir().expect("isolated server environment");
    let root = environment.path().join("workspace");
    let provider = root.join("Provider.pas");
    let main = root.join("Main.pas");
    let unrelated = root.join("Unrelated.pas");
    let provider_source = "unit Provider;\ninterface\nprocedure PublicRoutine;\nimplementation\nprocedure PublicRoutine;\nbegin\nend;\nend.\n";
    let main_source = "unit Main;\ninterface\nuses Provider;\nimplementation\nprocedure Run;\nbegin\n  PublicRoutine;\nend;\nend.\n";
    let unrelated_source = "unit Unrelated;\ninterface\nimplementation\nend.\n";
    write_file(&provider, provider_source);
    write_file(&main, main_source);
    write_file(&unrelated, unrelated_source);

    let (mut server, barrier) = TestServer::launch_with_navigation_barrier(environment);
    server.initialize(&root, Value::Null);

    let request_id = RequestId::from("unrelated-open-navigation".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/definition",
        navigation_params(&main, main_source, "PublicRoutine", 0),
    );
    barrier.wait_until_entered();

    server.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri(&unrelated),
                "languageId": "pascal",
                "version": 1,
                "text": unrelated_source
            }
        }),
    );
    server.send_notification(
        "textDocument/didChange",
        json!({
            "textDocument": {"uri": uri(&unrelated), "version": 2},
            "contentChanges": [{"text": format!("{unrelated_source}{{$IFDEF UNRELATED}}\n") }]
        }),
    );

    barrier.release();
    let locations = result_locations(server.response(&request_id));
    assert_eq!(locations.len(), 1);
    assert_eq!(locations[0]["uri"], uri(&provider).to_string());
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn requested_open_document_change_discards_blocked_navigation_result() {
    let environment = tempfile::tempdir().expect("isolated server environment");
    let root = environment.path().join("workspace");
    let main = root.join("Main.pas");
    let first_source = "unit Main;\ninterface\nprocedure Run;\nimplementation\nprocedure Run;\nbegin\nend;\nend.\n";
    let second_source =
        first_source.replace("procedure Run;", "procedure Changed;\nprocedure Run;");
    write_file(&main, first_source);

    let (mut server, barrier) = TestServer::launch_with_navigation_barrier(environment);
    server.initialize(&root, Value::Null);
    server.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri(&main),
                "languageId": "pascal",
                "version": 1,
                "text": first_source
            }
        }),
    );

    let request_id = RequestId::from("requested-open-navigation".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/definition",
        navigation_params(&main, first_source, "Run", 0),
    );
    barrier.wait_until_entered();
    server.send_notification(
        "textDocument/didChange",
        json!({
            "textDocument": {"uri": uri(&main), "version": 2},
            "contentChanges": [{"text": second_source}]
        }),
    );

    barrier.release();
    let response = server.response(&request_id);
    let error = response
        .error
        .expect("requested source change must stale the result");
    assert_eq!(error.code, -32803);
    assert_eq!(
        error.message,
        "analysis result became stale; retry the request"
    );
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn changed_imported_provider_discards_blocked_navigation_result() {
    let environment = tempfile::tempdir().expect("isolated server environment");
    let root = environment.path().join("workspace");
    let provider = root.join("Provider.pas");
    let main = root.join("Main.pas");
    let provider_source = "unit Provider;\ninterface\nprocedure PublicRoutine;\nimplementation\nprocedure PublicRoutine;\nbegin\nend;\nend.\n";
    let changed_provider_source = provider_source.replace("PublicRoutine", "ChangedRoutine");
    let main_source = "unit Main;\ninterface\nuses Provider;\nimplementation\nprocedure Run;\nbegin\n  PublicRoutine;\nend;\nend.\n";
    write_file(&provider, provider_source);
    write_file(&main, main_source);

    let (mut server, barrier) = TestServer::launch_with_navigation_barrier(environment);
    server.initialize(&root, Value::Null);
    let request_id = RequestId::from("changed-provider-navigation".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/definition",
        navigation_params(&main, main_source, "PublicRoutine", 0),
    );
    barrier.wait_until_entered();

    write_file(&provider, &changed_provider_source);
    server.send_notification(
        "workspace/didChangeWatchedFiles",
        json!({"changes": [{"uri": uri(&provider), "type": 2}]}),
    );

    barrier.release();
    let response = server.response(&request_id);
    let error = response
        .error
        .expect("changed provider must stale the result");
    assert_eq!(error.code, -32803);
    assert_eq!(
        error.message,
        "analysis result became stale; retry the request"
    );
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn absent_provider_overlay_invalidates_blocked_empty_navigation_result() {
    let environment = tempfile::tempdir().expect("isolated server environment");
    let root = environment.path().join("workspace");
    let main = root.join("Main.pas");
    let provider = root.join("AbsentProvider.pas");
    let main_source = "unit Main;\ninterface\nuses AbsentProvider;\nimplementation\nprocedure Run;\nbegin\n  PublicRoutine;\nend;\nend.\n";
    let provider_source = "unit AbsentProvider;\ninterface\nprocedure PublicRoutine;\nimplementation\nprocedure PublicRoutine;\nbegin\nend;\nend.\n";
    write_file(&main, main_source);

    let (mut server, barrier) = TestServer::launch_with_navigation_barrier(environment);
    server.initialize(&root, Value::Null);

    let request_id = RequestId::from("absent-provider-navigation".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/definition",
        navigation_params(&main, main_source, "PublicRoutine", 0),
    );
    barrier.wait_until_entered();

    server.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri(&provider),
                "languageId": "pascal",
                "version": 1,
                "text": provider_source
            }
        }),
    );

    barrier.release();
    let response = server.response(&request_id);
    let error = response
        .error
        .expect("fulfilled negative provider lookup must stale the old result");
    assert_eq!(error.code, -32803);
    assert_eq!(
        error.message,
        "analysis result became stale; retry the request"
    );

    let fresh_request_id = RequestId::from("absent-provider-navigation-fresh".to_string());
    server.send_request(
        fresh_request_id.clone(),
        "textDocument/definition",
        navigation_params(&main, main_source, "PublicRoutine", 0),
    );
    let locations = result_locations(server.response(&fresh_request_id));
    assert_eq!(locations.len(), 1);
    assert_eq!(locations[0]["uri"], uri(&provider).to_string());
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn nested_absent_provider_overlay_invalidates_blocked_empty_navigation_result() {
    let environment = tempfile::tempdir().expect("isolated server environment");
    let root = environment.path().join("workspace");
    let nested_root = root.join("nested");
    let main = root.join("Main.pas");
    let provider = nested_root.join("ReviewTask12Provider.pas");
    let main_source = "unit Main;\ninterface\nuses ReviewTask12Provider;\nimplementation\nprocedure Run;\nbegin\n  ReviewTask12Routine;\nend;\nend.\n";
    let provider_source = "unit ReviewTask12Provider;\ninterface\nprocedure ReviewTask12Routine;\nimplementation\nprocedure ReviewTask12Routine;\nbegin\nend;\nend.\n";
    fs::create_dir_all(&nested_root).expect("existing nested source root");
    write_file(&main, main_source);

    let (mut server, barrier) = TestServer::launch_with_navigation_barrier(environment);
    server.initialize(&root, Value::Null);

    let request_id = RequestId::from("nested-absent-provider-navigation".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/definition",
        navigation_params(&main, main_source, "ReviewTask12Routine", 0),
    );
    barrier.wait_until_entered();

    server.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri(&provider),
                "languageId": "pascal",
                "version": 1,
                "text": provider_source
            }
        }),
    );

    barrier.release();
    let response = server.response(&request_id);
    let error = response
        .error
        .expect("nested fulfilled negative provider lookup must stale the old result");
    assert_eq!(error.code, -32803);
    assert_eq!(
        error.message,
        "analysis result became stale; retry the request"
    );

    let fresh_request_id = RequestId::from("nested-absent-provider-navigation-fresh".to_string());
    server.send_request(
        fresh_request_id.clone(),
        "textDocument/definition",
        navigation_params(&main, main_source, "ReviewTask12Routine", 0),
    );
    let locations = result_locations(server.response(&fresh_request_id));
    assert_eq!(locations.len(), 1);
    assert_eq!(locations[0]["uri"], uri(&provider).to_string());
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn incomplete_filename_catalogue_invalidates_absent_nested_provider_overlay() {
    const CATALOGUE_LIMIT: usize = 8;

    let environment = tempfile::tempdir().expect("isolated server environment");
    let root = environment.path().join("workspace");
    let existing_directory = root.join("crates");
    let main = root.join("Main.pas");
    let provider = existing_directory.join("ReviewTask12Provider.pas");
    let main_source = "unit Main;\ninterface\nuses ReviewTask12Provider;\nimplementation\nprocedure Run;\nbegin\n  ReviewTask12Routine;\nend;\nend.\n";
    let provider_source = "unit ReviewTask12Provider;\ninterface\nprocedure ReviewTask12Routine;\nimplementation\nprocedure ReviewTask12Routine;\nbegin\nend;\nend.\n";
    fs::create_dir_all(&existing_directory).expect("existing source directory");
    write_file(&main, main_source);
    for index in 0..CATALOGUE_LIMIT {
        fs::write(
            existing_directory.join(format!("CataloguePadding{index:05}.txt")),
            [],
        )
        .expect("catalogue padding file");
    }

    let (mut server, barrier) =
        TestServer::launch_with_navigation_barrier_and_filename_catalogue_limit(
            environment,
            CATALOGUE_LIMIT,
        );
    server.initialize(&root, Value::Null);

    let request_id = RequestId::from("incomplete-catalogue-provider-navigation".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/definition",
        navigation_params(&main, main_source, "ReviewTask12Routine", 0),
    );
    barrier.wait_until_entered();

    server.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri(&provider),
                "languageId": "pascal",
                "version": 1,
                "text": provider_source
            }
        }),
    );

    barrier.release();
    let response = server.response(&request_id);
    let error = response
        .error
        .expect("incomplete catalogue provider overlay must stale the old result");
    assert_eq!(error.code, -32803);
    assert_eq!(
        error.message,
        "analysis result became stale; retry the request"
    );

    let fresh_request_id =
        RequestId::from("incomplete-catalogue-provider-navigation-fresh".to_string());
    server.send_request(
        fresh_request_id.clone(),
        "textDocument/definition",
        navigation_params(&main, main_source, "ReviewTask12Routine", 0),
    );
    let locations = result_locations(server.response(&fresh_request_id));
    assert_eq!(locations.len(), 1);
    assert_eq!(locations[0]["uri"], uri(&provider).to_string());
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn unrelated_nested_and_outside_provider_changes_do_not_stale_blocked_navigation() {
    let environment = tempfile::tempdir().expect("isolated server environment");
    let root = environment.path().join("workspace");
    let nested_root = root.join("nested");
    let outside_root = environment.path().join("outside");
    let main = root.join("Main.pas");
    let unrelated_nested = nested_root.join("DifferentProvider.pas");
    let outside_provider = outside_root.join("ReviewTask12Provider.pas");
    let main_source = "unit Main;\ninterface\nuses ReviewTask12Provider;\nimplementation\nprocedure Run;\nbegin\n  ReviewTask12Routine;\nend;\nend.\n";
    let unrelated_source = "unit DifferentProvider;\ninterface\nimplementation\nend.\n";
    fs::create_dir_all(&nested_root).expect("existing nested source root");
    fs::create_dir_all(&outside_root).expect("existing outside source root");
    write_file(&main, main_source);

    let (mut server, barrier) = TestServer::launch_with_navigation_barrier(environment);
    server.initialize(&root, Value::Null);

    let request_id = RequestId::from("unrelated-provider-navigation".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/definition",
        navigation_params(&main, main_source, "ReviewTask12Routine", 0),
    );
    barrier.wait_until_entered();

    server.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri(&unrelated_nested),
                "languageId": "pascal",
                "version": 1,
                "text": unrelated_source
            }
        }),
    );
    let _ = server.notification("textDocument/publishDiagnostics");
    server.send_notification(
        "workspace/didChangeWatchedFiles",
        json!({"changes": [{"uri": uri(&outside_provider), "type": 1}]}),
    );
    let unrelated_ack = root.join("UnrelatedAck.pas");
    server.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri(&unrelated_ack),
                "languageId": "pascal",
                "version": 1,
                "text": "unit UnrelatedAck;\ninterface\nimplementation\nend.\n"
            }
        }),
    );
    let _ = server.notification("textDocument/publishDiagnostics");

    barrier.release();
    let response = server.response(&request_id);
    assert!(
        response.error.is_none(),
        "unrelated provider changes must not stale the result: {response:?}"
    );
    assert!(result_locations(response).is_empty());
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn excluded_nested_provider_change_does_not_stale_blocked_navigation() {
    let environment = tempfile::tempdir().expect("isolated server environment");
    let root = environment.path().join("workspace");
    let excluded_root = root.join("excluded");
    let main = root.join("Main.pas");
    let provider = excluded_root.join("ReviewTask12Provider.pas");
    let main_source = "unit Main;\ninterface\nuses ReviewTask12Provider;\nimplementation\nprocedure Run;\nbegin\n  ReviewTask12Routine;\nend;\nend.\n";
    fs::create_dir_all(&excluded_root).expect("existing excluded source root");
    write_file(&main, main_source);

    let (mut server, barrier) = TestServer::launch_with_navigation_barrier(environment);
    server.initialize(&root, json!({"exclude": ["excluded/**"]}));

    let request_id = RequestId::from("excluded-provider-navigation".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/definition",
        navigation_params(&main, main_source, "ReviewTask12Routine", 0),
    );
    barrier.wait_until_entered();

    server.send_notification(
        "workspace/didChangeWatchedFiles",
        json!({"changes": [{"uri": uri(&provider), "type": 1}]}),
    );
    let excluded_ack = root.join("ExcludedAck.pas");
    server.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri(&excluded_ack),
                "languageId": "pascal",
                "version": 1,
                "text": "unit ExcludedAck;\ninterface\nimplementation\nend.\n"
            }
        }),
    );
    let _ = server.notification("textDocument/publishDiagnostics");

    barrier.release();
    let response = server.response(&request_id);
    assert!(
        response.error.is_none(),
        "excluded provider changes must not stale the result: {response:?}"
    );
    assert!(result_locations(response).is_empty());
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn native_case_distinct_provider_changes_preserve_stale_invalidation_across_event_orders() {
    let main_source = "unit Main;\ninterface\nuses ReviewTask12Provider;\nimplementation\nprocedure Run;\nbegin\n  ReviewTask12Routine;\nend;\nend.\n";
    let provider_source = "unit ReviewTask12Provider;\ninterface\nprocedure ReviewTask12Routine;\nimplementation\nprocedure ReviewTask12Routine;\nbegin\nend;\nend.\n";

    for (label, allowed_first) in [("allowed-first", true), ("excluded-first", false)] {
        let environment = tempfile::tempdir().expect("isolated server environment");
        let root = environment.path().join("workspace");
        let nested_root = root.join("nested");
        let main = root.join("Main.pas");
        let allowed_provider = nested_root.join("ReviewTask12Provider.pas");
        let excluded_provider = nested_root.join("REVIEWTASK12PROVIDER.pas");
        fs::create_dir_all(&nested_root).expect("existing nested source root");
        write_file(&main, main_source);

        let (mut server, barrier) = TestServer::launch_with_navigation_barrier(environment);
        server.initialize(
            &root,
            json!({
                "exclude": ["nested/REVIEWTASK12PROVIDER.pas"]
            }),
        );

        let request_id = RequestId::from(format!("case-distinct-provider-{label}"));
        server.send_request(
            request_id.clone(),
            "textDocument/definition",
            navigation_params(&main, main_source, "ReviewTask12Routine", 0),
        );
        barrier.wait_until_entered();

        if allowed_first {
            server.send_notification(
                "textDocument/didOpen",
                json!({
                    "textDocument": {
                        "uri": uri(&allowed_provider),
                        "languageId": "pascal",
                        "version": 1,
                        "text": provider_source
                    }
                }),
            );
            let _ = server.notification("textDocument/publishDiagnostics");
            server.send_notification(
                "workspace/didChangeWatchedFiles",
                json!({
                    "changes": [{"uri": uri(&excluded_provider), "type": 1}]
                }),
            );
        } else {
            server.send_notification(
                "workspace/didChangeWatchedFiles",
                json!({
                    "changes": [{"uri": uri(&excluded_provider), "type": 1}]
                }),
            );
            server.send_notification(
                "textDocument/didOpen",
                json!({
                    "textDocument": {
                        "uri": uri(&allowed_provider),
                        "languageId": "pascal",
                        "version": 1,
                        "text": provider_source
                    }
                }),
            );
            let _ = server.notification("textDocument/publishDiagnostics");
        }

        let acknowledgement = root.join(format!("{label}-ack.pas"));
        server.send_notification(
            "textDocument/didOpen",
            json!({
                "textDocument": {
                    "uri": uri(&acknowledgement),
                    "languageId": "pascal",
                    "version": 1,
                    "text": "unit Acknowledgement;\ninterface\nimplementation\nend.\n"
                }
            }),
        );
        let _ = server.notification("textDocument/publishDiagnostics");

        barrier.release();
        let response = server.response(&request_id);
        let error = match response.error {
            Some(error) => error,
            None => panic!("{label}: native allowed change was lost: {response:?}"),
        };
        assert_eq!(error.code, -32803, "{label}");
        assert_eq!(
            error.message, "analysis result became stale; retry the request",
            "{label}"
        );

        let fresh_request_id = RequestId::from(format!("case-distinct-provider-{label}-fresh"));
        server.send_request(
            fresh_request_id.clone(),
            "textDocument/definition",
            navigation_params(&main, main_source, "ReviewTask12Routine", 0),
        );
        let locations = result_locations(server.response(&fresh_request_id));
        assert_eq!(locations.len(), 1, "{label}");
        assert_eq!(
            locations[0]["uri"],
            uri(&allowed_provider).to_string(),
            "{label}"
        );
        server.shutdown();
    }
}

#[cfg(feature = "test-support")]
#[test]
fn unrelated_configuration_change_does_not_discard_blocked_formatting_result() {
    let environment = tempfile::tempdir().expect("isolated server environment");
    let root = environment.path().join("workspace");
    let unrelated = root.join("unrelated");
    let main = root.join("Main.pas");
    let source = "unit Main;\ninterface\nprocedure Run;\nimplementation\nprocedure Run;\nbegin\nend;\nend.\n";
    let unrelated_config = unrelated.join(".lint4d.toml");
    write_file(&main, source);

    let (mut server, barrier) = TestServer::launch_with_formatting_barrier(environment);
    server.initialize(&root, Value::Null);
    let request_id = RequestId::from("unrelated-configuration-formatting".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/formatting",
        json!({
            "textDocument": {"uri": uri(&main)},
            "options": {"tabSize": 2, "insertSpaces": true}
        }),
    );
    barrier.wait_until_entered();

    write_file(&unrelated_config, "[rules]\nconstant-naming = \"off\"\n");
    server.send_notification(
        "workspace/didChangeWatchedFiles",
        json!({"changes": [{"uri": uri(&unrelated_config), "type": 1}]}),
    );

    barrier.release();
    let response = server.response(&request_id);
    assert!(
        response.error.is_none(),
        "unrelated configuration must not stale formatting: {response:?}"
    );
    assert!(response.result.is_some());
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn blocked_formatting_does_not_block_unrelated_lsp_requests() {
    let environment = tempfile::tempdir().expect("isolated server environment");
    let root = environment.path().join("workspace");
    let main = root.join("Main.pas");
    let source = "unit Main;\ninterface\nprocedure Run;\nimplementation\nprocedure Run;\nbegin\nend;\nend.\n";
    write_file(&main, source);

    let (mut server, barrier) = TestServer::launch_with_formatting_barrier(environment);
    server.initialize(&root, Value::Null);

    let formatting_id = RequestId::from("blocked-formatting".to_string());
    server.send_request(
        formatting_id.clone(),
        "textDocument/formatting",
        json!({
            "textDocument": {"uri": uri(&main)},
            "options": {"tabSize": 2, "insertSpaces": true}
        }),
    );
    barrier.wait_until_entered();

    let symbols_id = RequestId::from("while-formatting-is-blocked".to_string());
    server.send_request(
        symbols_id.clone(),
        "textDocument/documentSymbol",
        json!({"textDocument": {"uri": uri(&main)}}),
    );
    let symbols = server.response(&symbols_id);
    assert!(
        symbols.error.is_none(),
        "unrelated request was blocked by formatting: {symbols:?}"
    );
    assert!(
        symbols.result.is_some(),
        "document symbols must be returned"
    );

    barrier.release();
    let formatting = server.response(&formatting_id);
    assert!(
        formatting.error.is_none(),
        "formatting request failed: {formatting:?}"
    );
    assert!(
        formatting.result.is_some(),
        "formatting result must be returned"
    );
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn diagnostics_drop_a_stale_blocked_result_after_a_newer_document_version() {
    let environment = tempfile::tempdir().expect("isolated server environment");
    let root = environment.path().join("workspace");
    let main = root.join("Main.pas");
    let first_source = "unit Main;\ninterface\nconst\n  badConst = 1;\nimplementation\nend.\n";
    let changed_start = position_of(first_source, "badConst", 0);
    let changed_end = position_after(first_source, "badConst", 0);
    write_file(&main, first_source);
    write_file(
        &root.join(".lint4d.toml"),
        "[rules]\nconstant-naming = \"warning\"\n[rules.naming]\nconstant_style = \"PascalCase\"\n",
    );

    let (mut server, barrier) = TestServer::launch_with_diagnostics_barrier(environment);
    server.initialize(&root, Value::Null);
    server.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri(&main),
                "languageId": "pascal",
                "version": 1,
                "text": first_source
            }
        }),
    );
    barrier.wait_until_entered();

    server.send_notification(
        "textDocument/didChange",
        json!({
            "textDocument": {"uri": uri(&main), "version": 2},
            "contentChanges": [{
                "range": {"start": changed_start, "end": changed_end},
                "rangeLength": 8,
                "text": "GoodConst"
            }]
        }),
    );
    barrier.release();

    let diagnostics = diagnostics_for_uri(&mut server, &uri(&main));
    assert_eq!(diagnostics["version"], 2);
    assert!(
        diagnostics["diagnostics"].as_array().unwrap().is_empty(),
        "stale diagnostics from version 1 were published: {diagnostics}"
    );
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn full_analysis_queue_returns_explicit_overflow_without_starting_an_extra_worker() {
    let environment = tempfile::tempdir().expect("isolated server environment");
    let root = environment.path().join("workspace");
    let provider = root.join("Provider.pas");
    let main = root.join("Main.pas");
    let provider_source = "unit Provider;\ninterface\nprocedure PublicRoutine;\nimplementation\nprocedure PublicRoutine;\nbegin\nend;\nend.\n";
    let main_source = "unit Main;\ninterface\nuses Provider;\nimplementation\nprocedure Run;\nbegin\n  PublicRoutine;\nend;\nend.\n";
    write_file(&provider, provider_source);
    write_file(&main, main_source);

    let (mut server, barrier) = TestServer::launch_with_navigation_barrier(environment);
    server.initialize(&root, Value::Null);

    let first_id = RequestId::from("full-queue-first".to_string());
    let second_id = RequestId::from("full-queue-second".to_string());
    server.send_request(
        first_id.clone(),
        "textDocument/definition",
        navigation_params(&main, main_source, "PublicRoutine", 0),
    );
    server.send_request(
        second_id.clone(),
        "textDocument/definition",
        navigation_params(&main, main_source, "Run", 0),
    );
    barrier.wait_for_entries(2);

    let queued_ids = (0..31)
        .map(|index| RequestId::from(format!("full-queue-{index}")))
        .collect::<Vec<_>>();
    for (index, id) in queued_ids.iter().cloned().enumerate() {
        server.send_request(
            id,
            "workspace/symbol",
            json!({"query": format!("MissingSymbol{index}")}),
        );
    }
    let overflow_id = RequestId::from("full-queue-overflow".to_string());
    server.send_request(
        overflow_id.clone(),
        "workspace/symbol",
        json!({"query": "OverflowSymbol"}),
    );
    let overflow = server.response(&overflow_id);
    let error = overflow
        .error
        .expect("full queue must reject the overflow request");
    assert_eq!(error.code, -32803);
    assert_eq!(error.message, "analysis queue is full; retry the request");

    barrier.release();
    let first_locations = result_locations(server.response(&first_id));
    let second_locations = result_locations(server.response(&second_id));
    assert_eq!(first_locations.len(), 1);
    assert_eq!(second_locations.len(), 1);
    for id in queued_ids {
        let response = server.response(&id);
        assert!(
            response.error.is_none(),
            "queued request failed: {response:?}"
        );
    }
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn diagnostics_use_reserved_capacity_when_the_client_queue_is_full() {
    let environment = tempfile::tempdir().expect("isolated server environment");
    let root = environment.path().join("workspace");
    let main = root.join("Main.pas");
    let source = "unit Main;\ninterface\nconst\n  badConst = 1;\nprocedure Run;\nimplementation\nprocedure Run;\nbegin\nend;\nend.\n";
    write_file(&main, source);
    write_file(
        &root.join(".lint4d.toml"),
        "[rules]\nconstant-naming = \"warning\"\n[rules.naming]\nconstant_style = \"PascalCase\"\n",
    );

    let (mut server, navigation, diagnostics, dispatch) =
        TestServer::launch_with_navigation_and_diagnostics_barriers_and_dispatch_log(environment);
    server.initialize(&root, Value::Null);
    for (id, occurrence) in [
        ("diagnostic-capacity-blocker-first", 0),
        ("diagnostic-capacity-blocker-second", 1),
    ] {
        server.send_request(
            RequestId::from(id.to_string()),
            "textDocument/definition",
            navigation_params(&main, source, "Run", occurrence),
        );
    }
    navigation.wait_for_entries(2);

    let queued_ids = (0..31)
        .map(|index| RequestId::from(format!("diagnostic-capacity-client-{index}")))
        .collect::<Vec<_>>();
    for (index, id) in queued_ids.iter().cloned().enumerate() {
        server.send_request(
            id,
            "workspace/symbol",
            json!({"query": format!("DiagnosticCapacitySymbol{index}")}),
        );
    }
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
    thread::sleep(Duration::from_millis(350));

    navigation.release();
    let dispatches = dispatch.wait_for_entries(3);
    assert_eq!(
        &dispatches[..2],
        b"II",
        "the two blocking requests must dispatch first"
    );
    assert_eq!(
        dispatches[2], b'D',
        "diagnostics must occupy the reserved queue slot before client work"
    );
    diagnostics.wait_until_entered();
    diagnostics.release();

    let published = server
        .diagnostic_with_timeout(&uri(&main), IO_TIMEOUT)
        .expect("diagnostics must progress from reserved queue capacity");
    assert_eq!(published["version"], 1);
    assert!(
        published["diagnostics"]
            .as_array()
            .expect("diagnostics array")
            .iter()
            .any(|diagnostic| diagnostic["code"] == "constant-naming"),
        "expected the configured naming diagnostic: {published}"
    );
    for id in queued_ids {
        let response = server.response(&id);
        assert!(
            response.error.is_none(),
            "reserved-capacity client request failed: {response:?}"
        );
    }
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn reusing_a_cancelled_request_id_keeps_attached_work_and_response_ids_exact() {
    let environment = tempfile::tempdir().expect("isolated server environment");
    let root = environment.path().join("workspace");
    let provider = root.join("Provider.pas");
    let main = root.join("Main.pas");
    let provider_source = "unit Provider;\ninterface\nprocedure PublicRoutine;\nimplementation\nprocedure PublicRoutine;\nbegin\nend;\nend.\n";
    let main_source = "unit Main;\ninterface\nuses Provider;\nprocedure Run;\nimplementation\nprocedure Run;\nbegin\n  PublicRoutine;\nend;\nend.\n";
    write_file(&provider, provider_source);
    write_file(&main, main_source);

    let (mut server, navigation, formatting, dispatch) =
        TestServer::launch_with_navigation_and_formatting_barriers_and_dispatch_log(environment);
    server.initialize(&root, Value::Null);

    let reused_id = RequestId::from("reused-request-id".to_string());
    let attached_id = RequestId::from("attached-request-id".to_string());
    server.send_request(
        reused_id.clone(),
        "textDocument/definition",
        navigation_params(&main, main_source, "PublicRoutine", 0),
    );
    navigation.wait_until_entered();
    server.send_request(
        attached_id.clone(),
        "textDocument/definition",
        navigation_params(&main, main_source, "PublicRoutine", 0),
    );
    assert!(
        server
            .response_with_timeout(&attached_id, Duration::from_millis(100))
            .is_none(),
        "an attached request must wait for the shared worker"
    );

    server.send_notification("$/cancelRequest", json!({"id": reused_id.clone()}));
    let cancelled = server.response(&reused_id);
    assert_eq!(cancelled.id, reused_id);
    assert_eq!(cancelled.error.expect("cancellation error").code, -32800);

    // Reuse the canceled ID for a distinct client-backed computation while
    // the old computation remains alive for the attached request.
    server.send_request(
        reused_id.clone(),
        "textDocument/formatting",
        json!({
            "textDocument": {"uri": uri(&main)},
            "options": {"tabSize": 2, "insertSpaces": true}
        }),
    );
    formatting.wait_until_entered();

    let queued_id = RequestId::from("request-after-id-reuse".to_string());
    server.send_request(
        queued_id.clone(),
        "textDocument/documentSymbol",
        json!({"textDocument": {"uri": uri(&main)}}),
    );
    assert!(
        server
            .response_with_timeout(&queued_id, Duration::from_millis(100))
            .is_none(),
        "a third computation must remain queued while two workers are active"
    );
    assert_eq!(
        fs::read(&dispatch.path).expect("dispatch log before release"),
        b"IB",
        "request ID reuse must not dispatch an extra worker"
    );

    // Complete the reused-ID computation first. The attached navigation must
    // still be retained and must not be mistaken for this result.
    formatting.release();
    let formatting_response = server.response(&reused_id);
    assert_eq!(formatting_response.id, reused_id);
    assert!(
        formatting_response.error.is_none(),
        "reused request ID formatting failed: {formatting_response:?}"
    );
    let queued_response = server.response(&queued_id);
    assert_eq!(queued_response.id, queued_id);
    assert!(
        queued_response.error.is_none(),
        "request after ID reuse failed: {queued_response:?}"
    );

    navigation.release();
    let attached_response = server
        .response_with_timeout(&attached_id, Duration::from_secs(1))
        .expect("attached request must receive the old computation result");
    assert_eq!(attached_response.id, attached_id);
    let locations = result_locations(attached_response);
    assert_eq!(locations.len(), 1);
    assert_eq!(locations[0]["uri"], uri(&provider).to_string());
    server.assert_no_response(&attached_id);
    server.assert_no_response(&reused_id);
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn running_shared_client_recipients_are_bounded_and_cancellation_frees_capacity() {
    let environment = tempfile::tempdir().expect("isolated server environment");
    let root = environment.path().join("workspace");
    let provider = root.join("Provider.pas");
    let main = root.join("Main.pas");
    let provider_source = "unit Provider;\ninterface\nprocedure PublicRoutine;\nimplementation\nprocedure PublicRoutine;\nbegin\nend;\nend.\n";
    let main_source = "unit Main;\ninterface\nuses Provider;\nprocedure Run;\nimplementation\nprocedure Run;\nbegin\n  PublicRoutine;\nend;\nend.\n";
    write_file(&provider, provider_source);
    write_file(&main, main_source);

    let (mut server, barrier) = TestServer::launch_with_navigation_barrier(environment);
    server.initialize(&root, Value::Null);

    let blocker_id = RequestId::from("running-recipient-blocker".to_string());
    server.send_request(
        blocker_id.clone(),
        "textDocument/definition",
        navigation_params(&provider, provider_source, "PublicRoutine", 0),
    );
    let primary_id = RequestId::from("running-recipient-primary".to_string());
    server.send_request(
        primary_id.clone(),
        "textDocument/definition",
        navigation_params(&main, main_source, "Run", 0),
    );
    barrier.wait_for_entries(2);

    let attachments = (0..31)
        .map(|index| RequestId::from(format!("running-recipient-attachment-{index}")))
        .collect::<Vec<_>>();
    for id in &attachments {
        server.send_request(
            id.clone(),
            "textDocument/definition",
            navigation_params(&main, main_source, "Run", 0),
        );
    }
    let overflow_same = RequestId::from("running-recipient-overflow-same".to_string());
    server.send_request(
        overflow_same.clone(),
        "textDocument/definition",
        navigation_params(&main, main_source, "Run", 0),
    );
    assert_queue_overflow(
        server
            .response_with_timeout(&overflow_same, Duration::from_secs(1))
            .expect("same-query recipient overflow response"),
    );

    let overflow_distinct = RequestId::from("running-recipient-overflow-distinct".to_string());
    server.send_request(
        overflow_distinct.clone(),
        "textDocument/documentSymbol",
        json!({"textDocument": {"uri": uri(&main)}}),
    );
    assert_queue_overflow(
        server
            .response_with_timeout(&overflow_distinct, Duration::from_secs(1))
            .expect("distinct recipient overflow response"),
    );

    let cancelled_id = attachments[0].clone();
    server.send_notification("$/cancelRequest", json!({"id": cancelled_id.clone()}));
    let cancelled = server.response(&cancelled_id);
    assert_eq!(
        cancelled.error.expect("attachment cancellation").code,
        -32800
    );

    let replacement_id = RequestId::from("running-recipient-replacement".to_string());
    server.send_request(
        replacement_id.clone(),
        "textDocument/definition",
        navigation_params(&main, main_source, "Run", 0),
    );

    barrier.release();
    assert!(
        server.response(&blocker_id).error.is_none(),
        "running blocker failed"
    );
    assert!(
        server.response(&primary_id).error.is_none(),
        "running shared primary failed"
    );
    for id in attachments.into_iter().skip(1) {
        assert!(
            server.response(&id).error.is_none(),
            "running shared attachment failed: {id:?}"
        );
    }
    assert!(
        server.response(&replacement_id).error.is_none(),
        "replacement recipient failed"
    );
    server.assert_no_response(&cancelled_id);
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn queued_shared_client_recipients_are_bounded_and_cancellation_frees_capacity() {
    let environment = tempfile::tempdir().expect("isolated server environment");
    let root = environment.path().join("workspace");
    let provider = root.join("Provider.pas");
    let main = root.join("Main.pas");
    let provider_source = "unit Provider;\ninterface\nprocedure PublicRoutine;\nimplementation\nprocedure PublicRoutine;\nbegin\nend;\nend.\n";
    let main_source = "unit Main;\ninterface\nuses Provider;\nprocedure Run;\nimplementation\nprocedure Run;\nbegin\n  PublicRoutine;\nend;\nend.\n";
    write_file(&provider, provider_source);
    write_file(&main, main_source);

    let (mut server, barrier) = TestServer::launch_with_navigation_barrier(environment);
    server.initialize(&root, Value::Null);

    let blocker_ids = [
        RequestId::from("queued-recipient-blocker-provider".to_string()),
        RequestId::from("queued-recipient-blocker-main".to_string()),
    ];
    server.send_request(
        blocker_ids[0].clone(),
        "textDocument/definition",
        navigation_params(&provider, provider_source, "PublicRoutine", 0),
    );
    server.send_request(
        blocker_ids[1].clone(),
        "textDocument/definition",
        navigation_params(&main, main_source, "PublicRoutine", 0),
    );
    barrier.wait_for_entries(2);

    let primary_id = RequestId::from("queued-recipient-primary".to_string());
    let target = navigation_params(&main, main_source, "Run", 0);
    server.send_request(
        primary_id.clone(),
        "textDocument/definition",
        target.clone(),
    );
    let attachments = (0..30)
        .map(|index| RequestId::from(format!("queued-recipient-attachment-{index}")))
        .collect::<Vec<_>>();
    for id in &attachments {
        server.send_request(id.clone(), "textDocument/definition", target.clone());
    }
    let overflow_same = RequestId::from("queued-recipient-overflow-same".to_string());
    server.send_request(
        overflow_same.clone(),
        "textDocument/definition",
        target.clone(),
    );
    assert_queue_overflow(
        server
            .response_with_timeout(&overflow_same, Duration::from_secs(1))
            .expect("queued same-query recipient overflow response"),
    );

    let overflow_distinct = RequestId::from("queued-recipient-overflow-distinct".to_string());
    server.send_request(
        overflow_distinct.clone(),
        "textDocument/documentSymbol",
        json!({"textDocument": {"uri": uri(&main)}}),
    );
    assert_queue_overflow(
        server
            .response_with_timeout(&overflow_distinct, Duration::from_secs(1))
            .expect("queued distinct recipient overflow response"),
    );

    let cancelled_id = attachments[0].clone();
    server.send_notification("$/cancelRequest", json!({"id": cancelled_id.clone()}));
    let cancelled = server.response(&cancelled_id);
    assert_eq!(
        cancelled
            .error
            .expect("queued attachment cancellation")
            .code,
        -32800
    );

    let replacement_id = RequestId::from("queued-recipient-replacement".to_string());
    server.send_request(replacement_id.clone(), "textDocument/definition", target);

    barrier.release();
    for id in blocker_ids {
        assert!(
            server.response(&id).error.is_none(),
            "queued blocker failed: {id:?}"
        );
    }
    assert!(
        server.response(&primary_id).error.is_none(),
        "queued shared primary failed"
    );
    for id in attachments.into_iter().skip(1) {
        assert!(
            server.response(&id).error.is_none(),
            "queued shared attachment failed: {id:?}"
        );
    }
    assert!(
        server.response(&replacement_id).error.is_none(),
        "queued replacement recipient failed"
    );
    server.assert_no_response(&cancelled_id);
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn full_client_queue_rejects_coalesced_recipients_without_unbounded_shutdown_responses() {
    let environment = tempfile::tempdir().expect("isolated server environment");
    let root = environment.path().join("workspace");
    let provider = root.join("Provider.pas");
    let main = root.join("Main.pas");
    let provider_source = "unit Provider;\ninterface\nprocedure PublicRoutine;\nimplementation\nprocedure PublicRoutine;\nbegin\nend;\nend.\n";
    let main_source = "unit Main;\ninterface\nuses Provider;\nprocedure Run;\nimplementation\nprocedure Run;\nbegin\n  PublicRoutine;\nend;\nend.\n";
    write_file(&provider, provider_source);
    write_file(&main, main_source);

    let (mut server, barrier) = TestServer::launch_with_navigation_barrier(environment);
    server.initialize(&root, Value::Null);

    let blocker_ids = [
        RequestId::from("full-recipient-blocker-provider".to_string()),
        RequestId::from("full-recipient-blocker-main".to_string()),
    ];
    server.send_request(
        blocker_ids[0].clone(),
        "textDocument/definition",
        navigation_params(&provider, provider_source, "PublicRoutine", 0),
    );
    server.send_request(
        blocker_ids[1].clone(),
        "textDocument/definition",
        navigation_params(&main, main_source, "PublicRoutine", 0),
    );
    barrier.wait_for_entries(2);

    let primary_id = RequestId::from("full-recipient-primary".to_string());
    let target = navigation_params(&main, main_source, "Run", 0);
    server.send_request(
        primary_id.clone(),
        "textDocument/definition",
        target.clone(),
    );
    let bulk_ids = (0..30)
        .map(|index| RequestId::from(format!("full-recipient-bulk-{index}")))
        .collect::<Vec<_>>();
    for (index, id) in bulk_ids.iter().cloned().enumerate() {
        server.send_request(
            id,
            "workspace/symbol",
            json!({"query": format!("FullRecipientMissing{index}")}),
        );
    }

    let attachment_ids = (0..1000)
        .map(|index| RequestId::from(format!("full-recipient-attachment-{index}")))
        .collect::<Vec<_>>();
    for id in &attachment_ids {
        server.send_request(id.clone(), "textDocument/definition", target.clone());
    }
    for id in attachment_ids {
        assert_queue_overflow(
            server
                .response_with_timeout(&id, Duration::from_secs(1))
                .expect("every excess coalesced recipient must be rejected"),
        );
    }

    let distinct_overflow = RequestId::from("full-recipient-distinct-overflow".to_string());
    server.send_request(
        distinct_overflow.clone(),
        "workspace/symbol",
        json!({"query": "FullRecipientOverflow"}),
    );
    assert_queue_overflow(
        server
            .response_with_timeout(&distinct_overflow, Duration::from_secs(1))
            .expect("full client queue distinct overflow response"),
    );

    barrier.release();
    for id in blocker_ids {
        assert!(
            server.response(&id).error.is_none(),
            "full client queue blocker failed: {id:?}"
        );
    }
    assert!(
        server.response(&primary_id).error.is_none(),
        "full client queue primary failed"
    );
    for id in bulk_ids {
        assert!(
            server.response(&id).error.is_none(),
            "full client queue bulk request failed: {id:?}"
        );
    }
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn bounded_analysis_queue_prioritizes_interactive_work_over_bulk_work() {
    let environment = tempfile::tempdir().expect("isolated server environment");
    let root = environment.path().join("workspace");
    let provider = root.join("Provider.pas");
    let main = root.join("Main.pas");
    let provider_source = "unit Provider;\ninterface\nprocedure PublicRoutine;\nimplementation\nprocedure PublicRoutine;\nbegin\nend;\nend.\n";
    let main_source = "unit Main;\ninterface\nuses Provider;\nimplementation\nprocedure Run;\nbegin\n  PublicRoutine;\nend;\nend.\n";
    write_file(&provider, provider_source);
    write_file(&main, main_source);

    let (mut server, barrier, dispatch) =
        TestServer::launch_with_navigation_barrier_and_dispatch_log(environment);
    server.initialize(&root, Value::Null);

    let first_id = RequestId::from("priority-first".to_string());
    let second_id = RequestId::from("priority-second".to_string());
    server.send_request(
        first_id.clone(),
        "textDocument/definition",
        navigation_params(&main, main_source, "PublicRoutine", 0),
    );
    server.send_request(
        second_id.clone(),
        "textDocument/definition",
        navigation_params(&main, main_source, "Run", 0),
    );
    barrier.wait_for_entries(2);

    let bulk_id = RequestId::from("priority-bulk".to_string());
    server.send_request(
        bulk_id.clone(),
        "textDocument/documentSymbol",
        json!({"textDocument": {"uri": uri(&main)}}),
    );
    let interactive_id = RequestId::from("priority-interactive".to_string());
    server.send_request(
        interactive_id.clone(),
        "textDocument/hover",
        navigation_params(&main, main_source, "PublicRoutine", 0),
    );
    assert!(
        server
            .response_with_timeout(&bulk_id, Duration::from_millis(100))
            .is_none(),
        "bulk work must remain queued while both workers are occupied"
    );
    assert!(
        server
            .response_with_timeout(&interactive_id, Duration::from_millis(100))
            .is_none(),
        "interactive work must remain queued while both workers are occupied"
    );

    barrier.release();
    let dispatches = dispatch.wait_for_entries(4);
    assert!(
        dispatches.len() >= 4,
        "all queued requests must be dispatched: {dispatches:?}"
    );
    assert_eq!(
        &dispatches[..2],
        b"II",
        "the two blockers must dispatch first"
    );
    assert_eq!(dispatches[2], b'I', "interactive work must dispatch first");
    assert_eq!(
        dispatches[3], b'B',
        "bulk work must dispatch after interactive work"
    );
    let bulk_response = server.response(&bulk_id);
    let interactive_response = server.response(&interactive_id);
    assert!(
        bulk_response.error.is_none(),
        "bulk response failed: {bulk_response:?}"
    );
    assert!(
        interactive_response.error.is_none(),
        "interactive response failed: {interactive_response:?}"
    );
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn cancelling_a_queued_request_removes_it_without_starting_a_worker() {
    let environment = tempfile::tempdir().expect("isolated server environment");
    let root = environment.path().join("workspace");
    let provider = root.join("Provider.pas");
    let main = root.join("Main.pas");
    let provider_source = "unit Provider;\ninterface\nprocedure PublicRoutine;\nimplementation\nprocedure PublicRoutine;\nbegin\nend;\nend.\n";
    let main_source = "unit Main;\ninterface\nuses Provider;\nimplementation\nprocedure Run;\nbegin\n  PublicRoutine;\nend;\nend.\n";
    write_file(&provider, provider_source);
    write_file(&main, main_source);

    let (mut server, barrier) = TestServer::launch_with_navigation_barrier(environment);
    server.initialize(&root, Value::Null);
    server.send_request(
        RequestId::from("queued-cancel-first".to_string()),
        "textDocument/definition",
        navigation_params(&main, main_source, "PublicRoutine", 0),
    );
    server.send_request(
        RequestId::from("queued-cancel-second".to_string()),
        "textDocument/definition",
        navigation_params(&main, main_source, "Run", 0),
    );
    barrier.wait_for_entries(2);

    let queued_id = RequestId::from("queued-cancelled".to_string());
    server.send_request(
        queued_id.clone(),
        "textDocument/documentSymbol",
        json!({"textDocument": {"uri": uri(&main)}}),
    );
    assert!(
        server
            .response_with_timeout(&queued_id, Duration::from_millis(100))
            .is_none(),
        "queued request must not respond before cancellation"
    );
    server.send_notification("$/cancelRequest", json!({"id": queued_id.clone()}));
    let cancelled = server
        .response_with_timeout(&queued_id, Duration::from_secs(1))
        .expect("queued cancellation response");
    assert_eq!(cancelled.error.expect("cancellation error").code, -32800);

    barrier.release();
    let _ = server.response(&RequestId::from("queued-cancel-first".to_string()));
    let _ = server.response(&RequestId::from("queued-cancel-second".to_string()));
    server.assert_no_response(&queued_id);
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn identical_queued_observations_share_one_worker_but_distinct_positions_do_not() {
    let environment = tempfile::tempdir().expect("isolated server environment");
    let root = environment.path().join("workspace");
    let provider = root.join("Provider.pas");
    let main = root.join("Main.pas");
    let provider_source = "unit Provider;\ninterface\nprocedure PublicRoutine;\nimplementation\nprocedure PublicRoutine;\nbegin\nend;\nend.\n";
    let main_source = "unit Main;\ninterface\nuses Provider;\nimplementation\nprocedure Run;\nbegin\n  PublicRoutine;\nend;\nend.\n";
    write_file(&provider, provider_source);
    write_file(&main, main_source);

    let (mut same_server, same_barrier) = TestServer::launch_with_navigation_barrier(environment);
    same_server.initialize(&root, Value::Null);
    same_server.send_request(
        RequestId::from("coalesce-first".to_string()),
        "textDocument/definition",
        navigation_params(&provider, provider_source, "PublicRoutine", 0),
    );
    same_server.send_request(
        RequestId::from("coalesce-second".to_string()),
        "textDocument/definition",
        navigation_params(&main, main_source, "Run", 0),
    );
    same_barrier.wait_for_entries(2);
    let first = RequestId::from("coalesced-query-first".to_string());
    let second = RequestId::from("coalesced-query-second".to_string());
    for id in [first.clone(), second.clone()] {
        same_server.send_request(
            id,
            "textDocument/definition",
            navigation_params(&main, main_source, "PublicRoutine", 0),
        );
    }
    same_barrier.release();
    same_barrier.wait_for_entries(3);
    thread::sleep(Duration::from_millis(100));
    assert_eq!(
        fs::read(&same_barrier.entered)
            .expect("coalesced barrier entries")
            .len(),
        3,
        "identical observations must share one dispatched worker"
    );
    let first_response = same_server.response(&first);
    let second_response = same_server.response(&second);
    assert!(
        first_response.error.is_none(),
        "first coalesced response failed"
    );
    assert!(
        second_response.error.is_none(),
        "second coalesced response failed"
    );
    same_server.assert_no_response(&first);
    same_server.assert_no_response(&second);
    same_server.shutdown();

    let environment = tempfile::tempdir().expect("isolated server environment");
    let root = environment.path().join("workspace");
    let provider = root.join("Provider.pas");
    let main = root.join("Main.pas");
    write_file(&provider, provider_source);
    write_file(&main, main_source);
    let (mut distinct_server, distinct_barrier) =
        TestServer::launch_with_navigation_barrier(environment);
    distinct_server.initialize(&root, Value::Null);
    distinct_server.send_request(
        RequestId::from("distinct-first".to_string()),
        "textDocument/definition",
        navigation_params(&provider, provider_source, "PublicRoutine", 0),
    );
    distinct_server.send_request(
        RequestId::from("distinct-second".to_string()),
        "textDocument/definition",
        navigation_params(&main, main_source, "Main", 0),
    );
    distinct_barrier.wait_for_entries(2);
    let first = RequestId::from("distinct-position-first".to_string());
    let second = RequestId::from("distinct-position-second".to_string());
    distinct_server.send_request(
        first.clone(),
        "textDocument/definition",
        navigation_params(&main, main_source, "PublicRoutine", 0),
    );
    distinct_server.send_request(
        second.clone(),
        "textDocument/definition",
        navigation_params(&main, main_source, "Run", 0),
    );
    distinct_barrier.release();
    distinct_barrier.wait_for_entries(4);
    assert!(
        distinct_server.response(&first).error.is_none(),
        "first distinct-position response failed"
    );
    assert!(
        distinct_server.response(&second).error.is_none(),
        "second distinct-position response failed"
    );
    distinct_server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn queued_analysis_dispatches_against_the_latest_document_snapshot() {
    let environment = tempfile::tempdir().expect("isolated server environment");
    let root = environment.path().join("workspace");
    let main = root.join("Main.pas");
    let first_source = "unit Main;\ninterface\nprocedure OldThing;\nimplementation\nend.\n";
    let second_source = "unit Main;\ninterface\nprocedure NewThing;\nimplementation\nend.\n";
    write_file(&main, first_source);

    let (mut server, barrier) = TestServer::launch_with_navigation_barrier(environment);
    server.initialize(&root, Value::Null);
    server.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri(&main),
                "languageId": "pascal",
                "version": 1,
                "text": first_source
            }
        }),
    );
    server.send_request(
        RequestId::from("latest-snapshot-first".to_string()),
        "textDocument/definition",
        navigation_params(&main, first_source, "OldThing", 0),
    );
    server.send_request(
        RequestId::from("latest-snapshot-second".to_string()),
        "textDocument/definition",
        json!({
            "textDocument": {"uri": uri(&main)},
            "position": {"line": 0, "character": 0}
        }),
    );
    barrier.wait_for_entries(2);

    let symbols_id = RequestId::from("latest-snapshot-symbols".to_string());
    server.send_request(
        symbols_id.clone(),
        "textDocument/documentSymbol",
        json!({"textDocument": {"uri": uri(&main)}}),
    );
    server.send_notification(
        "textDocument/didChange",
        json!({
            "textDocument": {"uri": uri(&main), "version": 2},
            "contentChanges": [{"text": second_source}]
        }),
    );
    assert!(
        server
            .response_with_timeout(&symbols_id, Duration::from_millis(100))
            .is_none(),
        "queued analysis must wait for a worker slot"
    );

    barrier.release();
    let symbols = server.response(&symbols_id);
    assert!(
        symbols.error.is_none(),
        "latest snapshot request failed: {symbols:?}"
    );
    let symbols = symbols.result.expect("document symbols");
    let symbols = symbols.as_array().expect("document symbol array");
    assert!(
        symbols.iter().any(|symbol| symbol["name"] == "NewThing"),
        "queued analysis returned the obsolete source snapshot: {symbols:?}"
    );
    assert!(
        symbols.iter().all(|symbol| symbol["name"] != "OldThing"),
        "queued analysis retained an obsolete declaration: {symbols:?}"
    );
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn newer_document_version_supersedes_a_queued_observation_once() {
    let environment = tempfile::tempdir().expect("isolated server environment");
    let root = environment.path().join("workspace");
    let provider = root.join("Provider.pas");
    let main = root.join("Main.pas");
    let provider_source = "unit Provider;\ninterface\nprocedure PublicRoutine;\nprocedure ChangedRoutine;\nimplementation\nprocedure PublicRoutine;\nbegin\nend;\nprocedure ChangedRoutine;\nbegin\nend;\nend.\n";
    let first_source = "unit Main;\ninterface\nuses Provider;\nimplementation\nprocedure Run;\nbegin\n  PublicRoutine;\nend;\nend.\n";
    let second_source = first_source.replace("PublicRoutine", "ChangedRoutine");
    write_file(&provider, provider_source);
    write_file(&main, first_source);

    let (mut server, barrier) = TestServer::launch_with_navigation_barrier(environment);
    server.initialize(&root, Value::Null);
    server.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri(&main),
                "languageId": "pascal",
                "version": 1,
                "text": first_source
            }
        }),
    );
    server.send_request(
        RequestId::from("supersede-blocker-main".to_string()),
        "textDocument/definition",
        navigation_params(&main, first_source, "Run", 0),
    );
    server.send_request(
        RequestId::from("supersede-blocker-provider".to_string()),
        "textDocument/definition",
        navigation_params(&provider, provider_source, "PublicRoutine", 0),
    );
    barrier.wait_for_entries(2);

    let old_id = RequestId::from("superseded-old".to_string());
    server.send_request(
        old_id.clone(),
        "textDocument/definition",
        navigation_params(&main, first_source, "PublicRoutine", 0),
    );
    server.send_notification(
        "textDocument/didChange",
        json!({
            "textDocument": {"uri": uri(&main), "version": 2},
            "contentChanges": [{"text": second_source}]
        }),
    );
    let new_id = RequestId::from("superseded-new".to_string());
    server.send_request(
        new_id.clone(),
        "textDocument/definition",
        navigation_params(&main, &second_source, "ChangedRoutine", 0),
    );

    let old_response = server
        .response_with_timeout(&old_id, Duration::from_secs(1))
        .expect("superseded request response");
    assert_eq!(old_response.error.expect("superseded error").code, -32800);
    server.assert_no_response(&old_id);

    barrier.release();
    let new_response = server.response(&new_id);
    assert!(
        new_response.error.is_none(),
        "new request failed: {new_response:?}"
    );
    assert_eq!(
        result_locations(new_response).len(),
        1,
        "newer version must still be dispatched"
    );
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn shutdown_cancels_queued_requests_without_dispatching_them() {
    let environment = tempfile::tempdir().expect("isolated server environment");
    let root = environment.path().join("workspace");
    let provider = root.join("Provider.pas");
    let main = root.join("Main.pas");
    let provider_source = "unit Provider;\ninterface\nprocedure PublicRoutine;\nimplementation\nprocedure PublicRoutine;\nbegin\nend;\nend.\n";
    let main_source = "unit Main;\ninterface\nuses Provider;\nimplementation\nprocedure Run;\nbegin\n  PublicRoutine;\nend;\nend.\n";
    write_file(&provider, provider_source);
    write_file(&main, main_source);

    let (mut server, barrier) = TestServer::launch_with_navigation_barrier(environment);
    server.initialize(&root, Value::Null);
    server.send_request(
        RequestId::from("shutdown-first".to_string()),
        "textDocument/definition",
        navigation_params(&main, main_source, "PublicRoutine", 0),
    );
    server.send_request(
        RequestId::from("shutdown-second".to_string()),
        "textDocument/definition",
        navigation_params(&main, main_source, "Run", 0),
    );
    barrier.wait_for_entries(2);
    let queued_id = RequestId::from("shutdown-queued".to_string());
    server.send_request(
        queued_id.clone(),
        "textDocument/documentSymbol",
        json!({"textDocument": {"uri": uri(&main)}}),
    );
    let shutdown_id = RequestId::from("shutdown-request".to_string());
    server.send_request(shutdown_id.clone(), "shutdown", Value::Null);

    let queued = server.response(&queued_id);
    assert_eq!(
        queued.error.expect("queued shutdown cancellation").code,
        -32800
    );
    let shutdown = server.response(&shutdown_id);
    assert!(shutdown.error.is_none(), "shutdown failed: {shutdown:?}");
    thread::sleep(Duration::from_millis(100));
    assert_eq!(
        fs::read(&barrier.entered)
            .expect("shutdown barrier entries")
            .len(),
        2,
        "shutdown must not dispatch a queued request"
    );
    server.send_notification("exit", Value::Null);
    server.stdin.take();
    let status = server.child.wait().expect("wait for shutdown server");
    assert!(status.success(), "server exited unsuccessfully: {status}");
}

#[cfg(feature = "test-support")]
#[test]
fn cancelling_blocked_navigation_returns_one_request_cancelled_response() {
    let environment = tempfile::tempdir().expect("isolated server environment");
    let root = environment.path().join("workspace");
    let provider = root.join("Provider.pas");
    let main = root.join("Main.pas");
    let provider_source = "unit Provider;\ninterface\nprocedure PublicRoutine;\nimplementation\nprocedure PublicRoutine;\nbegin\nend;\nend.\n";
    let main_source = "unit Main;\ninterface\nuses Provider;\nimplementation\nprocedure Run;\nbegin\n  PublicRoutine;\nend;\nend.\n";
    write_file(&provider, provider_source);
    write_file(&main, main_source);

    let (mut server, barrier) = TestServer::launch_with_navigation_barrier(environment);
    server.initialize(&root, Value::Null);
    let request_id = RequestId::from("cancelled-blocked-navigation".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/definition",
        navigation_params(&main, main_source, "PublicRoutine", 0),
    );
    barrier.wait_until_entered();
    server.send_notification(
        "$/cancelRequest",
        json!({"id": "cancelled-blocked-navigation"}),
    );

    let response = server.response(&request_id);
    let error = response
        .error
        .expect("cancelled navigation must return an error");
    assert_eq!(error.code, -32800);
    assert_eq!(error.message, "request cancelled");
    server.assert_no_response(&request_id);
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn cancelling_blocked_formatting_returns_one_request_cancelled_response() {
    let environment = tempfile::tempdir().expect("isolated server environment");
    let root = environment.path().join("workspace");
    let main = root.join("Main.pas");
    let source = "unit Main;\ninterface\nprocedure Run;\nimplementation\nprocedure Run;\nbegin\nend;\nend.\n";
    write_file(&main, source);

    let (mut server, barrier) = TestServer::launch_with_formatting_barrier(environment);
    server.initialize(&root, Value::Null);
    let request_id = RequestId::from("cancelled-blocked-formatting".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/formatting",
        json!({
            "textDocument": {"uri": uri(&main)},
            "options": {"tabSize": 2, "insertSpaces": true}
        }),
    );
    barrier.wait_until_entered();
    server.send_notification(
        "$/cancelRequest",
        json!({"id": "cancelled-blocked-formatting"}),
    );

    let response = server.response(&request_id);
    let error = response
        .error
        .expect("cancelled formatting must return an error");
    assert_eq!(error.code, -32800);
    assert_eq!(error.message, "request cancelled");
    server.assert_no_response(&request_id);
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn configuration_invalidation_discards_an_in_flight_diagnostic_result() {
    let environment = tempfile::tempdir().expect("isolated server environment");
    let root = environment.path().join("workspace");
    let main = root.join("Main.pas");
    let config = root.join(".lint4d.toml");
    let source = "unit Main;\ninterface\nconst\n  badConst = 1;\nimplementation\nend.\n";
    write_file(&main, source);
    write_file(
        &config,
        "[rules]\nconstant-naming = \"warning\"\n[rules.naming]\nconstant_style = \"PascalCase\"\n",
    );

    let (mut server, barrier) = TestServer::launch_with_diagnostics_barrier(environment);
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
    barrier.wait_until_entered();

    write_file(&config, "[rules]\nconstant-naming = \"off\"\n");
    server.send_notification(
        "workspace/didChangeWatchedFiles",
        json!({"changes": [{"uri": uri(&config), "type": 2}]}),
    );
    barrier.release();

    let diagnostics = diagnostics_for_uri(&mut server, &uri(&main));
    assert_eq!(diagnostics["version"], 1);
    assert!(
        diagnostics["diagnostics"]
            .as_array()
            .expect("diagnostics array")
            .iter()
            .all(|diagnostic| diagnostic["code"] != "constant-naming"),
        "diagnostics from the invalidated configuration were published: {diagnostics}"
    );
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn rapid_document_changes_coalesce_to_one_final_diagnostic_computation() {
    let environment = tempfile::tempdir().expect("isolated server environment");
    let root = environment.path().join("workspace");
    let main = root.join("Main.pas");
    let config = root.join(".lint4d.toml");
    let source_v1 = "unit Main;\ninterface\nconst\n  badConst = 1;\nimplementation\nend.\n";
    let source_v2 = source_v1.replace("badConst", "anotherConst");
    let source_v3 = source_v1.replace("badConst", "GoodConst");
    write_file(&main, source_v1);
    write_file(
        &config,
        "[rules]\nconstant-naming = \"warning\"\n[rules.naming]\nconstant_style = \"PascalCase\"\n",
    );

    let (mut server, barrier) = TestServer::launch_with_diagnostics_barrier(environment);
    server.initialize(&root, Value::Null);
    server.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri(&main),
                "languageId": "pascal",
                "version": 1,
                "text": source_v1
            }
        }),
    );
    barrier.wait_until_entered();
    barrier.release();
    let initial = diagnostics_for_uri(&mut server, &uri(&main));
    assert_eq!(initial["version"], 1);

    fs::remove_file(&barrier.entered).expect("reset diagnostic barrier entry");
    fs::remove_file(&barrier.release).expect("reset diagnostic barrier release");
    server.send_notification(
        "textDocument/didChange",
        json!({
            "textDocument": {"uri": uri(&main), "version": 2},
            "contentChanges": [{"text": source_v2}]
        }),
    );
    server.send_notification(
        "textDocument/didChange",
        json!({
            "textDocument": {"uri": uri(&main), "version": 3},
            "contentChanges": [{"text": source_v3}]
        }),
    );
    barrier.wait_until_entered();
    let entries = fs::read(&barrier.entered).expect("read diagnostic barrier entries");
    assert_eq!(
        entries.len(),
        1,
        "rapid changes must not start more than one final diagnostic computation"
    );
    barrier.release();

    let diagnostics = diagnostics_for_uri(&mut server, &uri(&main));
    assert_eq!(diagnostics["version"], 3);
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn shutdown_cancels_a_blocked_analysis_worker_before_exiting() {
    let environment = tempfile::tempdir().expect("isolated server environment");
    let root = environment.path().join("workspace");
    let provider = root.join("Provider.pas");
    let main = root.join("Main.pas");
    let provider_source = "unit Provider;\ninterface\nprocedure PublicRoutine;\nimplementation\nprocedure PublicRoutine;\nbegin\nend;\nend.\n";
    let main_source = "unit Main;\ninterface\nuses Provider;\nimplementation\nprocedure Run;\nbegin\n  PublicRoutine;\nend;\nend.\n";
    write_file(&provider, provider_source);
    write_file(&main, main_source);

    let (mut server, barrier) = TestServer::launch_with_navigation_barrier(environment);
    server.initialize(&root, Value::Null);
    let request_id = RequestId::from("shutdown-blocked-navigation".to_string());
    server.send_request(
        request_id,
        "textDocument/definition",
        navigation_params(&main, main_source, "PublicRoutine", 0),
    );
    barrier.wait_until_entered();
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn unrelated_open_notification_does_not_cancel_main_diagnostics() {
    let environment = tempfile::tempdir().expect("isolated server environment");
    let root = environment.path().join("workspace");
    let main = root.join("Main.pas");
    let other = root.join("Other.pas");
    let source = "unit Main;\ninterface\nconst badConst = 1;\nimplementation\nend.\n";
    let other_source = "unit Other;\ninterface\nimplementation\nend.\n";
    write_file(&main, source);
    write_file(&other, other_source);
    write_file(
        &root.join(".lint4d.toml"),
        "[rules]\nconstant-naming = \"warning\"\n[rules.naming]\nconstant_style = \"PascalCase\"\n",
    );

    let (mut server, barrier) = TestServer::launch_with_diagnostics_barrier(environment);
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
    barrier.wait_until_entered();

    server.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri(&other),
                "languageId": "pascal",
                "version": 1,
                "text": other_source
            }
        }),
    );
    barrier.release();

    let diagnostics = server
        .diagnostic_with_timeout(&uri(&main), Duration::from_secs(5))
        .expect("Main diagnostics after unrelated open");
    assert_eq!(diagnostics["version"], 1);
    assert!(
        diagnostics["diagnostics"]
            .as_array()
            .expect("diagnostics array")
            .iter()
            .any(|diagnostic| diagnostic["code"] == "constant-naming")
    );
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn unknown_notification_does_not_cancel_main_diagnostics() {
    let environment = tempfile::tempdir().expect("isolated server environment");
    let root = environment.path().join("workspace");
    let main = root.join("Main.pas");
    let source = "unit Main;\ninterface\nconst badConst = 1;\nimplementation\nend.\n";
    write_file(&main, source);
    write_file(
        &root.join(".lint4d.toml"),
        "[rules]\nconstant-naming = \"warning\"\n[rules.naming]\nconstant_style = \"PascalCase\"\n",
    );

    let (mut server, barrier) = TestServer::launch_with_diagnostics_barrier(environment);
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
    barrier.wait_until_entered();
    server.send_notification("$/setTrace", json!({"value": "off"}));
    barrier.release();

    let diagnostics = server
        .diagnostic_with_timeout(&uri(&main), Duration::from_secs(5))
        .expect("Main diagnostics after unrelated trace notification");
    assert_eq!(diagnostics["version"], 1);
    assert!(
        diagnostics["diagnostics"]
            .as_array()
            .expect("diagnostics array")
            .iter()
            .any(|diagnostic| diagnostic["code"] == "constant-naming")
    );
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn client_request_id_cannot_collide_with_internal_diagnostic_job() {
    let environment = tempfile::tempdir().expect("isolated server environment");
    let root = environment.path().join("workspace");
    let main = root.join("Main.pas");
    let source = "unit Main;\ninterface\nprocedure Run;\nimplementation\nprocedure Run;\nbegin\nend;\nend.\n";
    write_file(&main, source);

    let barrier_directory = environment.path().join("analysis-barriers");
    fs::create_dir_all(&barrier_directory).expect("barrier directory");
    let diagnostic_entered = barrier_directory.join("diagnostics.entered");
    let diagnostic_release = barrier_directory.join("diagnostics.release");
    let formatting_entered = barrier_directory.join("formatting.entered");
    let formatting_release = barrier_directory.join("formatting.release");
    let diagnostic_value = format!(
        "{}|{}",
        diagnostic_entered.display(),
        diagnostic_release.display()
    );
    let formatting_value = format!(
        "{}|{}",
        formatting_entered.display(),
        formatting_release.display()
    );
    let executable = env!("CARGO_BIN_EXE_pascal-lsp-test-server");
    let child = Command::new(executable)
        .arg("--stdio")
        .env("HOME", environment.path().join("home"))
        .env("XDG_CONFIG_HOME", environment.path().join("config"))
        .env("PASCAL_LSP_TEST_DIAGNOSTICS_BARRIER", &diagnostic_value)
        .env("PASCAL_LSP_TEST_FORMATTING_BARRIER", &formatting_value)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("launch pascal-lsp");
    let mut server = TestServer::from_child(child);
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
    wait_for_path(&diagnostic_entered);

    let colliding_id = RequestId::from("pascal-lsp-diagnostics-0".to_string());
    server.send_request(
        colliding_id.clone(),
        "textDocument/formatting",
        json!({
            "textDocument": {"uri": uri(&main)},
            "options": {"tabSize": 2, "insertSpaces": true}
        }),
    );
    wait_for_path(&formatting_entered);
    fs::write(&diagnostic_release, "release").expect("release diagnostics barrier");
    thread::sleep(Duration::from_millis(100));

    let ping_id = RequestId::from("collision-ping".to_string());
    server.send_request(ping_id.clone(), "review/ping", Value::Null);
    let ping = server
        .response_with_timeout(&ping_id, Duration::from_secs(1))
        .expect("unrecognized request must remain responsive");
    assert_eq!(ping.error.expect("unknown method error").code, -32601);

    server.send_notification("$/cancelRequest", json!({"id": colliding_id}));
    fs::write(&formatting_release, "release").expect("release formatting barrier");
    let formatting = server
        .response_with_timeout(&colliding_id, Duration::from_secs(1))
        .expect("colliding client request must receive one response");
    assert_eq!(
        formatting.error.expect("cancelled formatting error").code,
        -32800
    );
    server.assert_no_response(&colliding_id);
}

#[cfg(feature = "test-support")]
#[test]
fn production_binary_built_with_test_support_does_not_activate_test_barrier_from_environment() {
    let environment = tempfile::tempdir().expect("isolated server environment");
    let root = environment.path().join("workspace");
    let main = root.join("Main.pas");
    let source = "unit Main;\ninterface\nprocedure Run;\nimplementation\nprocedure Run;\nbegin\nend;\nend.\n";
    write_file(&main, source);
    let entered = environment.path().join("formatting.entered");
    let release = environment.path().join("formatting.release");
    let value = format!("{}|{}", entered.display(), release.display());
    let mut server = TestServer::launch_with_environment_path_and_variable(
        environment.path(),
        Some("PASCAL_LSP_TEST_FORMATTING_BARRIER"),
        Some(&value),
    );
    server.initialize(&root, Value::Null);
    let request_id = RequestId::from("feature-enabled-production-barrier-probe".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/formatting",
        json!({
            "textDocument": {"uri": uri(&main)},
            "options": {"tabSize": 2, "insertSpaces": true}
        }),
    );
    let response = server
        .response_with_timeout(&request_id, Duration::from_secs(1))
        .expect("production binary must not stall on test barrier environment");
    assert!(response.error.is_none(), "formatting failed: {response:?}");
    assert!(
        !entered.exists(),
        "production binary built with test-support wrote a test barrier marker"
    );
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn cancellation_takes_precedence_over_stale_generation_for_formatting() {
    let environment = tempfile::tempdir().expect("isolated server environment");
    let root = environment.path().join("workspace");
    let main = root.join("Main.pas");
    let source = "unit Main;\ninterface\nprocedure Run;\nimplementation\nprocedure Run;\nbegin\nend;\nend.\n";
    write_file(&main, source);

    let (mut server, barrier) = TestServer::launch_with_formatting_barrier(environment);
    server.initialize(&root, Value::Null);
    let request_id = RequestId::from("stale-formatting-cancellation".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/formatting",
        json!({
            "textDocument": {"uri": uri(&main)},
            "options": {"tabSize": 2, "insertSpaces": true}
        }),
    );
    barrier.wait_until_entered();
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
    server.send_notification("$/cancelRequest", json!({"id": request_id}));

    let response = server
        .response_with_timeout(&request_id, Duration::from_secs(1))
        .expect("cancelled formatting must respond");
    assert_eq!(response.error.expect("cancellation error").code, -32800);
    server.assert_no_response(&request_id);
}
