#![allow(dead_code)] // Each integration-test root uses a different subset of this shared harness.

use std::collections::{HashSet, VecDeque};
#[cfg(target_os = "linux")]
use std::ffi::CString;
use std::fs;
use std::io::{self, BufReader};
#[cfg(target_os = "linux")]
use std::os::fd::FromRawFd;
#[cfg(target_os = "linux")]
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
#[cfg(feature = "test-support")]
use std::sync::Arc;
#[cfg(feature = "test-support")]
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::RecvTimeoutError;
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use lsp_server::{Message, Notification, Request, RequestId, Response};
use lsp_types::{Position, TextEdit, Url};
use pascal_lsp::text;
use pascal_lsp::workspace::{Workspace, WorkspaceOptions};
use pascal_project::delphi_overrides::OverrideSession;
use serde_json::{Value, json};
use tempfile::TempDir;

#[cfg(target_os = "linux")]
unsafe extern "C" {
    pub(super) fn inotify_init1(flags: i32) -> i32;
    pub(super) fn inotify_add_watch(
        fd: i32,
        pathname: *const std::os::raw::c_char,
        mask: u32,
    ) -> i32;
    fn utimensat(
        dirfd: i32,
        pathname: *const std::os::raw::c_char,
        times: *const Timespec,
        flags: i32,
    ) -> i32;
}

#[cfg(target_os = "linux")]
pub(super) const IN_CLOSE_NOWRITE: u32 = 0x0000_0010;

#[cfg(target_os = "linux")]
pub(super) const IN_OPEN: u32 = 0x0000_0020;

#[cfg(target_os = "linux")]
pub(super) const IN_NONBLOCK: i32 = 0x800;

#[cfg(target_os = "linux")]
pub(super) const AT_FDCWD: i32 = -100;

#[cfg(target_os = "linux")]
pub(super) const UTIME_OMIT: i64 = 1_073_741_822;

#[cfg(target_os = "linux")]
#[repr(C)]
pub(super) struct Timespec {
    pub(super) tv_sec: i64,
    pub(super) tv_nsec: i64,
}

#[cfg(target_os = "linux")]
pub(super) fn wait_for_close_events(fd: i32, expected: usize) {
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
pub(super) fn observed_open(path: &Path, operation: impl FnOnce()) -> bool {
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
pub(super) fn restore_mtime(path: &Path, metadata: &fs::Metadata) {
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

// Only Linux observes file opens in these tests, so elsewhere the file may be
// opened to set its timestamp.
#[cfg(not(target_os = "linux"))]
pub(super) fn restore_mtime(path: &Path, metadata: &fs::Metadata) {
    let modified = metadata.modified().expect("modification time");
    fs::File::options()
        .write(true)
        .open(path)
        .and_then(|file| file.set_modified(modified))
        .unwrap_or_else(|error| panic!("restore mtime for {}: {error}", path.display()));
}

// The serial protocol suite starts many real server processes. Allow for
// scheduler stalls on loaded hosts without weakening the short, explicit
// negative-assertion timeouts used below.
pub(super) const IO_TIMEOUT: Duration = Duration::from_secs(10);
/// Requests that deliberately drive a server limit to its maximum in a debug
/// build, such as 32 missing-unit binding proofs.
pub(super) const LIMIT_REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

pub(super) fn test_workspace(roots: Vec<PathBuf>, options: WorkspaceOptions) -> Workspace {
    Workspace::with_override_session(roots, options, OverrideSession::new(None))
}

pub(super) struct TestServer {
    pub(super) child: Child,
    pub(super) stdin: Option<ChildStdin>,
    pub(super) messages: Receiver<io::Result<Option<Message>>>,
    pub(super) pending: VecDeque<Message>,
    pub(super) _environment: Option<TempDir>,
    #[cfg(feature = "test-support")]
    pub(super) stdout_paused: Arc<AtomicBool>,
}

#[cfg(feature = "test-support")]
pub(super) struct TestBarrier {
    pub(super) entered: PathBuf,
    pub(super) release: PathBuf,
}

#[cfg(feature = "test-support")]
pub(super) struct OutboundWriterBarrier {
    pub(super) armed: PathBuf,
    pub(super) entered: PathBuf,
    pub(super) release: PathBuf,
    pub(super) control_limit: usize,
}

#[cfg(feature = "test-support")]
pub(super) struct TestDispatchLog {
    pub(super) path: PathBuf,
}

impl TestServer {
    pub(super) fn launch() -> Self {
        Self::launch_with_environment(tempfile::tempdir().expect("isolated server environment"))
    }

    pub(super) fn launch_with_environment(environment: TempDir) -> Self {
        let mut server = Self::launch_with_environment_path(environment.path());
        server._environment = Some(environment);
        server
    }

    pub(super) fn launch_with_environment_path(environment: &Path) -> Self {
        Self::launch_with_environment_path_and_variable(environment, None, None)
    }

    #[cfg(feature = "test-support")]
    pub(super) fn launch_with_analysis_metrics(environment: TempDir) -> Self {
        let mut server = Self::launch_test_server_with_environment_path_and_variable(
            environment.path(),
            Some("PASCAL_LSP_ANALYSIS_METRICS"),
            Some("1"),
        );
        server._environment = Some(environment);
        server
    }

    #[cfg(feature = "test-support")]
    pub(super) fn launch_with_navigation_barrier(environment: TempDir) -> (Self, TestBarrier) {
        Self::launch_with_barrier(environment, "PASCAL_LSP_TEST_NAVIGATION_BARRIER")
    }

    #[cfg(feature = "test-support")]
    pub(super) fn launch_with_outbound_writer_barrier(
        environment: TempDir,
    ) -> (Self, OutboundWriterBarrier) {
        let directory = environment.path().join("outbound-writer-barrier");
        fs::create_dir_all(&directory).expect("outbound writer barrier directory");
        let barrier = OutboundWriterBarrier {
            armed: directory.join("armed"),
            entered: directory.join("entered"),
            release: directory.join("release"),
            control_limit: 8,
        };
        let value = format!(
            "{}|{}|{}|{}",
            barrier.armed.display(),
            barrier.entered.display(),
            barrier.release.display(),
            barrier.control_limit
        );
        let mut server = Self::launch_test_server_with_environment_path_and_variable(
            environment.path(),
            Some("PASCAL_LSP_TEST_OUTBOUND_WRITER_BARRIER"),
            Some(value.as_str()),
        );
        server._environment = Some(environment);
        (server, barrier)
    }

    #[cfg(feature = "test-support")]
    pub(super) fn launch_with_file_event_failure(environment: &TempDir, uri: &Url) -> Self {
        Self::launch_test_server_with_environment_path_and_variable(
            environment.path(),
            Some("PASCAL_LSP_TEST_FILE_EVENT_FAIL_AFTER_MUTATION_URI"),
            Some(uri.as_str()),
        )
    }

    #[cfg(feature = "test-support")]
    pub(super) fn launch_with_navigation_barrier_and_configuration(
        environment: TempDir,
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
        let mut server = Self::launch_test_server_with_environment_path_and_variable(
            environment.path(),
            Some("PASCAL_LSP_TEST_NAVIGATION_BARRIER"),
            Some(barrier_value.as_str()),
        );
        server._environment = Some(environment);
        (server, barrier)
    }

    #[cfg(feature = "test-support")]
    pub(super) fn launch_with_configuration_preparation_barrier(
        environment: TempDir,
    ) -> (Self, TestBarrier) {
        let barrier_directory = environment.path().join("configuration-barrier");
        fs::create_dir_all(&barrier_directory).expect("configuration barrier directory");
        let barrier = TestBarrier {
            entered: barrier_directory.join("entered"),
            release: barrier_directory.join("release"),
        };
        let barrier_value = format!(
            "{}|{}",
            barrier.entered.display(),
            barrier.release.display()
        );
        let mut server = Self::launch_test_server_with_environment_path_and_variable(
            environment.path(),
            Some("PASCAL_LSP_TEST_CONFIGURATION_PREPARATION_BARRIER"),
            Some(barrier_value.as_str()),
        );
        server._environment = Some(environment);
        (server, barrier)
    }

    #[cfg(feature = "test-support")]
    pub(super) fn launch_with_project_operation_barrier(
        environment: TempDir,
    ) -> (Self, TestBarrier) {
        let directory = environment.path().join("project-operation-barrier");
        fs::create_dir_all(&directory).expect("project operation barrier directory");
        let barrier = TestBarrier {
            entered: directory.join("entered"),
            release: directory.join("release"),
        };
        let value = format!(
            "{}|{}",
            barrier.entered.display(),
            barrier.release.display()
        );
        let mut server = Self::launch_test_server_with_environment_path_and_variable(
            environment.path(),
            Some("PASCAL_LSP_TEST_PROJECT_OPERATION_BARRIER"),
            Some(value.as_str()),
        );
        server._environment = Some(environment);
        (server, barrier)
    }

    #[cfg(feature = "test-support")]
    pub(super) fn launch_with_project_operation_prepared_barrier(
        environment: TempDir,
    ) -> (Self, TestBarrier) {
        let directory = environment
            .path()
            .join("project-operation-prepared-barrier");
        fs::create_dir_all(&directory).expect("prepared project operation barrier directory");
        let barrier = TestBarrier {
            entered: directory.join("entered"),
            release: directory.join("release"),
        };
        let value = format!(
            "{}|{}",
            barrier.entered.display(),
            barrier.release.display()
        );
        let mut server = Self::launch_test_server_with_environment_path_and_variable(
            environment.path(),
            Some("PASCAL_LSP_TEST_PROJECT_OPERATION_PREPARED_BARRIER"),
            Some(value.as_str()),
        );
        server._environment = Some(environment);
        (server, barrier)
    }

    #[cfg(feature = "test-support")]
    pub(super) fn launch_with_automatic_selection_prepared_barrier(
        environment: TempDir,
    ) -> (Self, TestBarrier) {
        let directory = environment
            .path()
            .join("automatic-selection-prepared-barrier");
        fs::create_dir_all(&directory).expect("automatic selection barrier directory");
        let barrier = TestBarrier {
            entered: directory.join("entered"),
            release: directory.join("release"),
        };
        let value = format!(
            "{}|{}",
            barrier.entered.display(),
            barrier.release.display()
        );
        let mut server = Self::launch_test_server_with_environment_path_and_variable(
            environment.path(),
            Some("PASCAL_LSP_TEST_AUTOMATIC_SELECTION_PREPARED_BARRIER"),
            Some(value.as_str()),
        );
        server._environment = Some(environment);
        (server, barrier)
    }

    #[cfg(feature = "test-support")]
    pub(super) fn launch_with_project_context_prepared_barrier(
        environment: TempDir,
    ) -> (Self, TestBarrier) {
        let directory = environment.path().join("project-context-prepared-barrier");
        fs::create_dir_all(&directory).expect("project context barrier directory");
        let barrier = TestBarrier {
            entered: directory.join("entered"),
            release: directory.join("release"),
        };
        let value = format!(
            "{}|{}",
            barrier.entered.display(),
            barrier.release.display()
        );
        let mut server = Self::launch_test_server_with_environment_path_and_variable(
            environment.path(),
            Some("PASCAL_LSP_TEST_PROJECT_CONTEXT_PREPARED_BARRIER"),
            Some(value.as_str()),
        );
        server._environment = Some(environment);
        (server, barrier)
    }

    #[cfg(feature = "test-support")]
    pub(super) fn launch_with_manual_selection_prepared_barrier(
        environment: TempDir,
    ) -> (Self, TestBarrier) {
        let directory = environment.path().join("manual-selection-prepared-barrier");
        fs::create_dir_all(&directory).expect("manual selection barrier directory");
        let barrier = TestBarrier {
            entered: directory.join("entered"),
            release: directory.join("release"),
        };
        let value = format!(
            "{}|{}",
            barrier.entered.display(),
            barrier.release.display()
        );
        let mut server = Self::launch_test_server_with_environment_path_and_variable(
            environment.path(),
            Some("PASCAL_LSP_TEST_MANUAL_SELECTION_PREPARED_BARRIER"),
            Some(value.as_str()),
        );
        server._environment = Some(environment);
        (server, barrier)
    }

    #[cfg(feature = "test-support")]
    pub(super) fn launch_with_compiled_content_barrier(
        environment: TempDir,
    ) -> (Self, TestBarrier) {
        let directory = environment.path().join("compiled-content-barrier");
        fs::create_dir_all(&directory).expect("compiled content barrier directory");
        let barrier = TestBarrier {
            entered: directory.join("entered"),
            release: directory.join("release"),
        };
        let value = format!(
            "{}|{}",
            barrier.entered.display(),
            barrier.release.display()
        );
        let mut server = Self::launch_test_server_with_environment_path_and_variable(
            environment.path(),
            Some("PASCAL_LSP_TEST_COMPILED_CONTENT_BARRIER"),
            Some(value.as_str()),
        );
        server._environment = Some(environment);
        (server, barrier)
    }

    #[cfg(feature = "test-support")]
    pub(super) fn launch_with_navigation_and_configuration_preparation_barriers(
        environment: TempDir,
    ) -> (Self, TestBarrier, TestBarrier) {
        let configuration_directory = environment.path().join("configuration-barrier");
        let navigation_directory = environment.path().join("navigation-barrier");
        fs::create_dir_all(&configuration_directory).expect("configuration barrier directory");
        fs::create_dir_all(&navigation_directory).expect("navigation barrier directory");
        let configuration = TestBarrier {
            entered: configuration_directory.join("entered"),
            release: configuration_directory.join("release"),
        };
        let navigation = TestBarrier {
            entered: navigation_directory.join("entered"),
            release: navigation_directory.join("release"),
        };
        let configuration_value = format!(
            "{}|{}",
            configuration.entered.display(),
            configuration.release.display()
        );
        let navigation_value = format!(
            "{}|{}",
            navigation.entered.display(),
            navigation.release.display()
        );
        let mut server = Self::launch_test_server_with_environment_path_and_variables(
            environment.path(),
            [
                (
                    "PASCAL_LSP_TEST_CONFIGURATION_PREPARATION_BARRIER",
                    configuration_value.as_str(),
                ),
                (
                    "PASCAL_LSP_TEST_NAVIGATION_BARRIER",
                    navigation_value.as_str(),
                ),
            ],
        );
        server._environment = Some(environment);
        (server, configuration, navigation)
    }

    #[cfg(feature = "test-support")]
    pub(super) fn launch_with_navigation_barrier_and_filename_catalogue_limit(
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
    pub(super) fn launch_with_navigation_barrier_and_dispatch_log(
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
    pub(super) fn launch_with_navigation_and_diagnostics_barriers_and_dispatch_log(
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
    pub(super) fn launch_with_navigation_and_formatting_barriers_and_dispatch_log(
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
    pub(super) fn launch_with_formatting_barrier(environment: TempDir) -> (Self, TestBarrier) {
        Self::launch_with_barrier(environment, "PASCAL_LSP_TEST_FORMATTING_BARRIER")
    }

    #[cfg(feature = "test-support")]
    pub(super) fn launch_with_diagnostics_barrier(environment: TempDir) -> (Self, TestBarrier) {
        Self::launch_with_barrier(environment, "PASCAL_LSP_TEST_DIAGNOSTICS_BARRIER")
    }

    #[cfg(feature = "test-support")]
    pub(super) fn launch_with_selection_barrier(environment: TempDir) -> (Self, TestBarrier) {
        Self::launch_with_barrier(environment, "PASCAL_LSP_TEST_SELECTION_BARRIER")
    }

    #[cfg(feature = "test-support")]
    pub(super) fn launch_with_completion_resolution_barrier(
        environment: TempDir,
    ) -> (Self, TestBarrier) {
        Self::launch_with_barrier(environment, "PASCAL_LSP_TEST_COMPLETION_RESOLUTION_BARRIER")
    }

    #[cfg(feature = "test-support")]
    pub(super) fn launch_with_workspace_symbols_barrier(
        environment: TempDir,
    ) -> (Self, TestBarrier) {
        Self::launch_with_barrier(environment, "PASCAL_LSP_TEST_WORKSPACE_SYMBOLS_BARRIER")
    }

    #[cfg(feature = "test-support")]
    pub(super) fn launch_with_partial_validation_barrier(
        environment: TempDir,
    ) -> (Self, TestBarrier) {
        Self::launch_with_barrier(environment, "PASCAL_LSP_TEST_PARTIAL_VALIDATION_BARRIER")
    }

    #[cfg(feature = "test-support")]
    pub(super) fn launch_with_workspace_symbols_and_diagnostics_barriers(
        environment: TempDir,
    ) -> (Self, TestBarrier, TestBarrier) {
        let workspace_directory = environment.path().join("workspace-symbols-barrier");
        let diagnostics_directory = environment.path().join("diagnostics-barrier");
        fs::create_dir_all(&workspace_directory).expect("workspace-symbols barrier directory");
        fs::create_dir_all(&diagnostics_directory).expect("diagnostics barrier directory");
        let workspace_symbols = TestBarrier {
            entered: workspace_directory.join("entered"),
            release: workspace_directory.join("release"),
        };
        let diagnostics = TestBarrier {
            entered: diagnostics_directory.join("entered"),
            release: diagnostics_directory.join("release"),
        };
        let workspace_symbols_value = format!(
            "{}|{}",
            workspace_symbols.entered.display(),
            workspace_symbols.release.display()
        );
        let diagnostics_value = format!(
            "{}|{}",
            diagnostics.entered.display(),
            diagnostics.release.display()
        );
        let mut server = Self::launch_test_server_with_environment_path_and_variables(
            environment.path(),
            [
                (
                    "PASCAL_LSP_TEST_WORKSPACE_SYMBOLS_BARRIER",
                    workspace_symbols_value.as_str(),
                ),
                (
                    "PASCAL_LSP_TEST_DIAGNOSTICS_BARRIER",
                    diagnostics_value.as_str(),
                ),
            ],
        );
        server._environment = Some(environment);
        (server, workspace_symbols, diagnostics)
    }

    #[cfg(feature = "test-support")]
    pub(super) fn launch_with_barrier(environment: TempDir, variable: &str) -> (Self, TestBarrier) {
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
    pub(super) fn launch_test_server_with_environment_path_and_variable(
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
    pub(super) fn launch_test_server_with_environment_path_and_variables<'a>(
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
            .stderr(Stdio::inherit())
            .envs(variables)
            .spawn()
            .expect("launch pascal-lsp test server");
        Self::from_child(child)
    }

    #[cfg(feature = "test-support")]
    pub(super) fn launch_test_server_with_environment_path_and_variables_and_stderr_file<'a>(
        environment: &Path,
        variables: impl IntoIterator<Item = (&'a str, &'a str)>,
        stderr_file: &Path,
    ) -> Self {
        let executable = env!("CARGO_BIN_EXE_pascal-lsp-test-server");
        let stderr = fs::File::create(stderr_file).expect("create test server stderr log");
        let child = Command::new(executable)
            .arg("--stdio")
            .env("HOME", environment.join("home"))
            .env("XDG_CONFIG_HOME", environment.join("config"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::from(stderr))
            .envs(variables)
            .spawn()
            .expect("launch pascal-lsp test server");
        Self::from_child(child)
    }

    pub(super) fn launch_with_environment_path_and_variable(
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
            .stderr(Stdio::inherit())
            .envs(variable.into_iter().zip(value))
            .spawn()
            .expect("launch pascal-lsp");
        Self::from_child(child)
    }

    pub(super) fn from_child(mut child: Child) -> Self {
        let stdout = child.stdout.take().expect("child stdout");
        let (sender, receiver) = mpsc::channel();
        #[cfg(feature = "test-support")]
        let stdout_paused = Arc::new(AtomicBool::new(false));
        #[cfg(feature = "test-support")]
        let reader_pause = Arc::clone(&stdout_paused);
        thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            loop {
                #[cfg(feature = "test-support")]
                while reader_pause.load(Ordering::Acquire) {
                    thread::sleep(Duration::from_millis(1));
                }
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
            #[cfg(feature = "test-support")]
            stdout_paused,
        }
    }

    pub(super) fn send_request(&mut self, id: impl Into<RequestId>, method: &str, params: Value) {
        self.send(Message::Request(Request::new(
            id.into(),
            method.to_string(),
            params,
        )));
    }

    pub(super) fn send_notification(&mut self, method: &str, params: Value) {
        self.send(Message::Notification(Notification::new(
            method.to_string(),
            params,
        )));
    }

    pub(super) fn send(&mut self, message: Message) {
        message
            .write(self.stdin.as_mut().expect("server stdin"))
            .expect("write LSP message");
    }

    #[cfg(feature = "test-support")]
    pub(super) fn send_tracked(&mut self, message: Message, sent: &mut Vec<Message>) {
        sent.push(message.clone());
        self.send(message);
    }

    pub(super) fn response(&mut self, expected_id: &RequestId) -> Response {
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
    pub(super) fn response_acknowledging_diagnostic_refresh(
        &mut self,
        expected_id: &RequestId,
    ) -> Response {
        let deadline = Instant::now() + IO_TIMEOUT;
        loop {
            if let Some(index) = self.pending.iter().position(|message| {
                matches!(message, Message::Request(request) if request.method == "workspace/diagnostic/refresh")
            }) {
                let Message::Request(refresh) =
                    self.pending.remove(index).expect("pending diagnostic refresh")
                else {
                    unreachable!("refresh request predicate");
                };
                self.send(Message::Response(Response::new_ok(refresh.id, Value::Null)));
                continue;
            }
            if let Some(index) = self.pending.iter().position(|message| {
                matches!(message, Message::Response(response) if &response.id == expected_id)
            }) {
                return match self.pending.remove(index).expect("pending response") {
                    Message::Response(response) => response,
                    _ => unreachable!("response predicate"),
                };
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            let message = match self.messages.recv_timeout(remaining) {
                Ok(Ok(Some(message))) => message,
                Ok(Ok(None)) | Err(RecvTimeoutError::Disconnected) => {
                    panic!("server disconnected waiting for {expected_id:?}")
                }
                Ok(Err(error)) => {
                    panic!("failed reading while waiting for {expected_id:?}: {error}")
                }
                Err(RecvTimeoutError::Timeout) => panic!(
                    "timed out waiting for {expected_id:?}; status {:?}, pending {:?}",
                    self.child.try_wait(),
                    self.pending
                ),
            };
            match message {
                Message::Request(request) if request.method == "workspace/diagnostic/refresh" => {
                    self.send(Message::Response(Response::new_ok(request.id, Value::Null)));
                }
                Message::Response(response) if &response.id == expected_id => return response,
                other => self.pending.push_back(other),
            }
        }
    }

    pub(super) fn response_with_timeout(
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
    pub(super) fn assert_no_response(&mut self, expected_id: &RequestId) {
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

    #[cfg(feature = "test-support")]
    pub(super) fn pause_stdout(&self) {
        self.stdout_paused.store(true, Ordering::Release);
    }

    #[cfg(feature = "test-support")]
    pub(super) fn resume_stdout(&self) {
        self.stdout_paused.store(false, Ordering::Release);
    }

    #[cfg(feature = "test-support")]
    pub(super) fn assert_no_progress(&mut self) {
        assert!(
            !self.pending.iter().any(|message| {
                matches!(
                    message,
                    Message::Notification(notification) if notification.method == "$/progress"
                )
            }),
            "unexpected progress notification in pending messages: {:?}",
            self.pending
        );
        let deadline = Instant::now() + Duration::from_millis(100);
        while Instant::now() < deadline {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match self.messages.recv_timeout(remaining) {
                Ok(Ok(Some(Message::Notification(notification))))
                    if notification.method == "$/progress" =>
                {
                    panic!("unexpected progress notification: {notification:?}");
                }
                Ok(Ok(Some(message))) => self.pending.push_back(message),
                Ok(Ok(None)) | Err(RecvTimeoutError::Disconnected) => return,
                Ok(Err(error)) => panic!("failed reading progress check: {error}"),
                Err(RecvTimeoutError::Timeout) => return,
            }
        }
    }

    #[cfg(feature = "test-support")]
    pub(super) fn assert_no_notification(&mut self, method: &str) {
        assert!(
            !self.pending.iter().any(|message| {
                matches!(
                    message,
                    Message::Notification(notification) if notification.method == method
                )
            }),
            "unexpected {method} notification in pending messages: {:?}",
            self.pending
        );
        let deadline = Instant::now() + Duration::from_millis(100);
        while Instant::now() < deadline {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match self.messages.recv_timeout(remaining) {
                Ok(Ok(Some(Message::Notification(notification))))
                    if notification.method == method =>
                {
                    panic!("unexpected {method} notification: {notification:?}");
                }
                Ok(Ok(Some(message))) => self.pending.push_back(message),
                Ok(Ok(None)) | Err(RecvTimeoutError::Disconnected) => return,
                Ok(Err(error)) => panic!("failed reading {method} check: {error}"),
                Err(RecvTimeoutError::Timeout) => return,
            }
        }
    }

    #[cfg(feature = "test-support")]
    pub(super) fn pending_request_count(&self, method: &str) -> usize {
        self.pending
            .iter()
            .filter(
                |message| matches!(message, Message::Request(request) if request.method == method),
            )
            .count()
    }

    #[cfg(feature = "test-support")]
    pub(super) fn assert_no_request(&self, method: &str) {
        let count = self.pending_request_count(method);
        assert_eq!(
            count, 0,
            "unexpected {count} buffered request(s) for {method}: {:?}",
            self.pending
        );
    }

    #[cfg(feature = "test-support")]
    pub(super) fn next_message(&mut self) -> Message {
        self.pending
            .pop_front()
            .unwrap_or_else(|| self.receive_until(Instant::now() + IO_TIMEOUT))
    }

    pub(super) fn notification(&mut self, method: &str) -> Value {
        self.notification_with_timeout(method, IO_TIMEOUT)
    }

    pub(super) fn notification_with_timeout(&mut self, method: &str, timeout: Duration) -> Value {
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

    pub(super) fn take_partial_items(&mut self, token: &str) -> Vec<Value> {
        let mut items = Vec::new();
        let mut retained = VecDeque::new();
        while let Some(message) = self.pending.pop_front() {
            match message {
                Message::Notification(notification)
                    if notification.method == "$/progress"
                        && notification.params.get("token").and_then(Value::as_str)
                            == Some(token) =>
                {
                    if let Some(chunk) = notification.params["value"].as_array() {
                        items.extend(chunk.iter().cloned());
                    }
                }
                other => retained.push_back(other),
            }
        }
        self.pending = retained;
        items
    }

    pub(super) fn diagnostic_with_timeout(
        &mut self,
        expected: &Url,
        timeout: Duration,
    ) -> Option<Value> {
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

    pub(super) fn request(&mut self, method: &str) -> Request {
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

    pub(super) fn request_with_timeout(
        &mut self,
        method: &str,
        timeout: Duration,
    ) -> Option<Request> {
        if let Some(index) = self.pending.iter().position(
            |message| matches!(message, Message::Request(request) if request.method == method),
        ) {
            return match self.pending.remove(index).expect("pending request") {
                Message::Request(request) => Some(request),
                _ => unreachable!("pending request predicate"),
            };
        }
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match self.messages.recv_timeout(remaining) {
                Ok(Ok(Some(Message::Request(request)))) if request.method == method => {
                    return Some(request);
                }
                Ok(Ok(Some(message))) => self.pending.push_back(message),
                Ok(Ok(None)) | Err(mpsc::RecvTimeoutError::Disconnected) => return None,
                Ok(Err(error)) => panic!("failed reading request: {error}"),
                Err(mpsc::RecvTimeoutError::Timeout) => return None,
            }
        }
        None
    }

    pub(super) fn receive_until(&mut self, deadline: Instant) -> Message {
        let remaining = deadline.saturating_duration_since(Instant::now());
        self.messages
            .recv_timeout(remaining)
            .expect("receive LSP message before timeout")
            .expect("read LSP message")
            .expect("LSP server closed unexpectedly")
    }

    pub(super) fn initialize(&mut self, root: &Path, initialization_options: Value) -> Value {
        self.initialize_with_watched_registration(root, initialization_options, false)
    }

    pub(super) fn initialize_with_pull_diagnostics(&mut self, root: &Path) -> Value {
        self.initialize_with_pull_diagnostics_options(root, Value::Null)
    }

    pub(super) fn initialize_with_pull_diagnostics_options(
        &mut self,
        root: &Path,
        initialization_options: Value,
    ) -> Value {
        let root_uri = Url::from_file_path(root).expect("workspace URI");
        let id = RequestId::from("pull-diagnostics-initialize".to_string());
        self.send_request(
            id.clone(),
            "initialize",
            json!({
                "processId": null,
                "rootUri": root_uri,
                "initializationOptions": initialization_options,
                "capabilities": {
                    "textDocument": {
                        "diagnostic": {
                            "dynamicRegistration": false,
                            "relatedDocumentSupport": true
                        }
                    },
                    "workspace": {
                        "diagnostics": {"refreshSupport": true},
                        "workspaceFolders": true
                    }
                }
            }),
        );
        let response = self.response(&id);
        assert!(response.error.is_none(), "initialize failed: {response:?}");
        self.send_notification("initialized", json!({}));
        response.result.expect("initialize result")
    }

    #[cfg(feature = "test-support")]
    pub(super) fn initialize_with_progress(&mut self, root: &Path) -> Value {
        let id = RequestId::from("progress-helper-initialize".to_string());
        self.send_request(
            id.clone(),
            "initialize",
            json!({
                "processId": null,
                "rootUri": uri(root),
                "capabilities": {"window": {"workDoneProgress": true}}
            }),
        );
        let response = self.response(&id);
        assert!(response.error.is_none(), "initialize failed: {response:?}");
        self.send_notification("initialized", json!({}));
        response.result.expect("initialize result")
    }

    pub(super) fn initialize_with_folding_capabilities(
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

    pub(super) fn initialize_with_inlay_hint_capabilities(
        &mut self,
        root: &Path,
        inlay_hint_capabilities: Value,
    ) -> Value {
        let root_uri = Url::from_file_path(root).expect("workspace URI");
        let id = RequestId::from("initialize-inlay-hint".to_string());
        self.send_request(
            id.clone(),
            "initialize",
            json!({
                "processId": null,
                "rootUri": root_uri,
                "capabilities": {
                    "textDocument": {"inlayHint": inlay_hint_capabilities}
                }
            }),
        );
        let response = self.response(&id);
        assert!(response.error.is_none(), "initialize failed: {response:?}");
        self.send_notification("initialized", json!({}));
        response.result.expect("initialize result")
    }

    #[cfg(unix)]
    pub(super) fn initialize_with_workspace_folders(
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

    pub(super) fn initialize_with_hierarchical_document_symbols(&mut self, root: &Path) -> Value {
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

    pub(super) fn initialize_with_watched_registration(
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

    pub(super) fn initialize_with_watched_registration_and_relative_patterns(
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

    pub(super) fn initialize_with_action_support(
        &mut self,
        root: &Path,
        initialization_options: Value,
    ) -> Value {
        self.initialize_with_client_capabilities(root, initialization_options, false, true)
    }

    pub(super) fn initialize_with_resolve_properties(
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

    pub(super) fn initialize_with_completion_resolve_properties(
        &mut self,
        root: &Path,
        properties: Value,
        documentation_formats: Value,
    ) -> Value {
        self.initialize_with_completion_capabilities(root, None, properties, documentation_formats)
    }

    pub(super) fn initialize_with_completion_capabilities(
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

    pub(super) fn initialize_without_document_changes(
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

    pub(super) fn initialize_with_client_capabilities(
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

    pub(super) fn initialize_with_client_capabilities_and_document_changes(
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

    pub(super) fn initialize_with_client_capabilities_and_document_changes_and_relative(
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

    pub(super) fn initialize_with_configuration_capability(
        &mut self,
        root: &Path,
        configuration: Option<bool>,
        initialization_options: Value,
    ) -> Value {
        let root_uri = Url::from_file_path(root).expect("workspace URI");
        let id = RequestId::from("configuration-initialize".to_string());
        let mut workspace = json!({
            "workspaceFolders": true,
        });
        if let Some(configuration) = configuration {
            workspace["configuration"] = json!(configuration);
        }
        self.send_request(
            id.clone(),
            "initialize",
            json!({
                "processId": null,
                "rootUri": root_uri,
                "initializationOptions": initialization_options,
                "capabilities": {
                    "workspace": workspace,
                }
            }),
        );
        let response = self.response(&id);
        assert!(response.error.is_none(), "initialize failed: {response:?}");
        self.send_notification("initialized", json!({}));
        response.result.expect("initialize result")
    }

    pub(super) fn initialize_with_pull_diagnostics_and_configuration(
        &mut self,
        root: &Path,
        configuration: bool,
    ) -> Value {
        let root_uri = Url::from_file_path(root).expect("workspace URI");
        let id = RequestId::from("pull-diagnostics-configuration-initialize".to_string());
        self.send_request(
            id.clone(),
            "initialize",
            json!({
                "processId": null,
                "rootUri": root_uri,
                "capabilities": {
                    "textDocument": {
                        "diagnostic": {
                            "dynamicRegistration": false,
                            "relatedDocumentSupport": true
                        }
                    },
                    "workspace": {
                        "diagnostics": {"refreshSupport": true},
                        "configuration": configuration,
                        "workspaceFolders": true
                    }
                }
            }),
        );
        let response = self.response(&id);
        assert!(response.error.is_none(), "initialize failed: {response:?}");
        self.send_notification("initialized", json!({}));
        response.result.expect("initialize result")
    }

    #[cfg(feature = "test-support")]
    pub(super) fn initialize_with_configuration_and_document_changes(
        &mut self,
        root: &Path,
        initialization_options: Value,
    ) -> Value {
        let root_uri = Url::from_file_path(root).expect("workspace URI");
        let id = RequestId::from("configuration-document-changes-initialize".to_string());
        self.send_request(
            id.clone(),
            "initialize",
            json!({
                "processId": null,
                "rootUri": root_uri,
                "initializationOptions": initialization_options,
                "capabilities": {
                    "workspace": {
                        "configuration": true,
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

    pub(super) fn initialize_with_configuration_capability_and_folders(
        &mut self,
        root: &Path,
        folders: &[&Path],
        configuration: bool,
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
        let id = RequestId::from("configuration-folders-initialize".to_string());
        self.send_request(
            id.clone(),
            "initialize",
            json!({
                "processId": null,
                "rootUri": root_uri,
                "workspaceFolders": workspace_folders,
                "capabilities": {
                    "workspace": {
                        "workspaceFolders": true,
                        "configuration": configuration,
                    }
                }
            }),
        );
        let response = self.response(&id);
        assert!(response.error.is_none(), "initialize failed: {response:?}");
        self.send_notification("initialized", json!({}));
        response.result.expect("initialize result")
    }

    pub(super) fn shutdown(&mut self) {
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
    // Queue-admission tests must occupy the interactive-only slot as well as
    // the two general workers before asserting that later work stays queued.
    pub(super) fn occupy_interactive_slot(&self, server: &mut TestServer, path: &Path) {
        server.send_request(
            RequestId::from("additional-interactive-slot-blocker".to_string()),
            "textDocument/declaration",
            json!({"textDocument": {"uri": uri(path)}, "position": {"line": 0, "character": 0}}),
        );
        self.wait_for_entries(3);
    }

    pub(super) fn wait_until_entered(&self) {
        self.wait_for_entries(1);
    }

    pub(super) fn wait_for_entries(&self, expected: usize) {
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

    pub(super) fn release(&self) {
        fs::write(&self.release, b"release").expect("release analysis barrier");
    }
}

#[cfg(feature = "test-support")]
impl OutboundWriterBarrier {
    pub(super) fn wait_until_entered(&self) {
        let deadline = Instant::now() + IO_TIMEOUT;
        while Instant::now() < deadline {
            if self.entered.exists() {
                return;
            }
            thread::sleep(Duration::from_millis(5));
        }
        panic!("outbound writer did not enter the test barrier");
    }

    pub(super) fn release(&self) {
        fs::write(&self.release, b"release").expect("release outbound writer barrier");
    }
}

#[cfg(feature = "test-support")]
impl TestDispatchLog {
    pub(super) fn wait_for_entries(&self, expected: usize) -> Vec<u8> {
        let deadline = Instant::now() + IO_TIMEOUT;
        while Instant::now() < deadline {
            if let Ok(entries) = fs::read(&self.path)
                && entries.len() >= expected
            {
                return entries;
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
pub(super) fn wait_for_path(path: &Path) {
    let deadline = Instant::now() + IO_TIMEOUT;
    while Instant::now() < deadline {
        if path.exists() {
            return;
        }
        thread::sleep(Duration::from_millis(5));
    }
    panic!("expected path to be created: {}", path.display());
}

pub(super) fn uri(path: &Path) -> Url {
    Url::from_file_path(path).expect("file URI")
}

pub(super) fn decoded_semantic_tokens(
    result: &Value,
    token_types: &[&str],
) -> Vec<(u32, u32, u32, String, u32)> {
    let data = result["data"].as_array().expect("semantic token data");
    let mut line = 0;
    let mut character = 0;
    let mut tokens = Vec::with_capacity(data.len() / 5);
    for chunk in data.as_chunks::<5>().0 {
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

/// The `window/logMessage` line a read-only request sends when its result
/// left out what it could not prove.
pub(super) fn incomplete_result_log(server: &mut TestServer, method: &str) -> String {
    let log = server.notification("window/logMessage");
    let message = log["message"]
        .as_str()
        .expect("log message text")
        .to_string();
    assert!(message.contains(method), "{message}");
    assert!(message.contains("incomplete result"), "{message}");
    message
}

pub(super) fn position_of(source: &str, needle: &str, occurrence: usize) -> Position {
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

pub(super) fn navigation_params(
    path: &Path,
    source: &str,
    needle: &str,
    occurrence: usize,
) -> Value {
    json!({
        "textDocument": {"uri": uri(path)},
        "position": position_of(source, needle, occurrence),
    })
}

pub(super) fn position_after(source: &str, needle: &str, occurrence: usize) -> Position {
    let start = position_of(source, needle, occurrence);
    Position::new(
        start.line,
        start.character + needle.encode_utf16().count() as u32,
    )
}

pub(super) fn apply_completion_item(source: &str, item: &Value) -> String {
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

pub(super) fn expand_lsp_snippet(snippet: &str) -> String {
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

pub(super) fn unescape_lsp_snippet_literal(value: &str) -> String {
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

pub(super) fn apply_expanded_completion_item(source: &str, item: &Value) -> String {
    let mut expanded = item.clone();
    let snippet = expanded["textEdit"]["newText"]
        .as_str()
        .expect("snippet text");
    expanded["textEdit"]["newText"] = json!(expand_lsp_snippet(snippet));
    apply_completion_item(source, &expanded)
}

pub(super) fn final_qualified_type_position(source: &str, qualified_name: &str) -> Position {
    let start = position_of(source, qualified_name, 0);
    let prefix = qualified_name
        .rsplit_once('.')
        .map_or(0, |(prefix, _)| prefix.len() + 1);
    Position::new(start.line, start.character + prefix as u32)
}

pub(super) fn result_locations(response: Response) -> Vec<Value> {
    assert!(response.error.is_none(), "request failed: {response:?}");
    response
        .result
        .expect("request result")
        .as_array()
        .expect("array location result")
        .clone()
}

pub(super) fn request_missing_unit_actions(
    server: &mut TestServer,
    path: &Path,
    source: &str,
    identifier: &str,
    request_id: &str,
) -> Vec<Value> {
    request_missing_unit_actions_with_timeout(
        server, path, source, identifier, request_id, IO_TIMEOUT,
    )
}

pub(super) fn request_missing_unit_actions_with_timeout(
    server: &mut TestServer,
    path: &Path,
    source: &str,
    identifier: &str,
    request_id: &str,
    timeout: Duration,
) -> Vec<Value> {
    let id = RequestId::from(request_id.to_string());
    server.send_request(
        id.clone(),
        "textDocument/codeAction",
        json!({
            "textDocument": {"uri": uri(path)},
            "range": {
                "start": position_of(source, identifier, 0),
                "end": position_after(source, identifier, 0),
            },
            "context": {"diagnostics": [], "only": ["quickfix"]}
        }),
    );
    let response = server
        .response_with_timeout(&id, timeout)
        .expect("code action response");
    assert!(response.error.is_none(), "codeAction failed: {response:?}");
    response
        .result
        .expect("code action result")
        .as_array()
        .expect("code action array")
        .clone()
}

#[cfg(feature = "test-support")]
pub(super) fn collect_partial_response(
    server: &mut TestServer,
    request_id: &RequestId,
    token: &Value,
) -> (Vec<Value>, Response) {
    let mut items = Vec::new();
    loop {
        match server.next_message() {
            Message::Notification(notification) if notification.method == "$/progress" => {
                assert_eq!(&notification.params["token"], token);
                let chunk = notification.params["value"]
                    .as_array()
                    .expect("partial progress value must be an array");
                assert!(!chunk.is_empty(), "partial chunks must not be empty");
                items.extend(chunk.iter().cloned());
            }
            Message::Response(response) if &response.id == request_id => {
                return (items, response);
            }
            other => server.pending.push_back(other),
        }
    }
}

#[cfg(feature = "test-support")]
pub(super) fn assert_queue_overflow(response: Response) {
    let error = response.error.expect("analysis queue overflow error");
    assert_eq!(error.code, -32803);
    assert_eq!(error.message, "analysis queue is full; retry the request");
}

pub(super) fn location_signature(location: &Value) -> (String, u32, u32, u32, u32) {
    (
        location["uri"].as_str().unwrap_or_default().to_owned(),
        location["range"]["start"]["line"].as_u64().unwrap() as u32,
        location["range"]["start"]["character"].as_u64().unwrap() as u32,
        location["range"]["end"]["line"].as_u64().unwrap() as u32,
        location["range"]["end"]["character"].as_u64().unwrap() as u32,
    )
}

#[cfg(feature = "test-support")]
pub(super) fn symbol_signature(symbol: &Value) -> (String, u32, u32, u32, u32) {
    let location = &symbol["location"];
    (
        location["uri"].as_str().unwrap_or_default().to_owned(),
        location["range"]["start"]["line"].as_u64().unwrap() as u32,
        location["range"]["start"]["character"].as_u64().unwrap() as u32,
        location["range"]["end"]["line"].as_u64().unwrap() as u32,
        location["range"]["end"]["character"].as_u64().unwrap() as u32,
    )
}

pub(super) fn range_signature(location: &Value) -> (u32, u32, u32, u32) {
    (
        location["range"]["start"]["line"].as_u64().unwrap() as u32,
        location["range"]["start"]["character"].as_u64().unwrap() as u32,
        location["range"]["end"]["line"].as_u64().unwrap() as u32,
        location["range"]["end"]["character"].as_u64().unwrap() as u32,
    )
}

pub(super) fn expected_location_signature(
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

pub(super) fn assert_exact_location_signatures(
    actual: &[Value],
    mut expected: Vec<(String, u32, u32, u32, u32)>,
) {
    let mut actual = actual.iter().map(location_signature).collect::<Vec<_>>();
    actual.sort();
    expected.sort();
    assert_eq!(actual, expected);
}

pub(super) fn write_file(path: &Path, source: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create source directory");
    }
    fs::write(path, source).expect("write Pascal source");
}

pub(super) fn write_bytes(path: &Path, source: &[u8]) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create source directory");
    }
    fs::write(path, source).expect("write Pascal source bytes");
}

pub(super) fn latin1(source: &str) -> Vec<u8> {
    source
        .chars()
        .map(|character| {
            let codepoint = character as u32;
            assert!(codepoint <= 0xff, "test source is not Latin-1");
            codepoint as u8
        })
        .collect()
}

pub(super) fn case_distinct_paths_supported(directory: &Path) -> bool {
    fs::create_dir_all(directory).expect("create case-sensitivity probe directory");
    let lower = directory.join("case-sensitivity-probe");
    let upper = directory.join("CASE-SENSITIVITY-PROBE");
    let _ = fs::remove_file(&lower);
    let _ = fs::remove_file(&upper);
    fs::write(&lower, b"lower").expect("write case-sensitivity probe");
    let distinct = !upper.exists();
    let _ = fs::remove_file(&lower);
    let _ = fs::remove_file(&upper);
    distinct
}

pub(super) fn workspace_symbol_source(unit_name: &str, variable_count: usize) -> String {
    let mut source = format!("unit {unit_name};\ninterface\nvar\n");
    for index in 0..variable_count {
        source.push_str(&format!("  Symbol{index}: Integer;\n"));
    }
    source.push_str("implementation\nend.\n");
    source
}

#[cfg(feature = "test-support")]
pub(super) fn wide_workspace_symbol_source(
    unit_name: &str,
    prefix: &str,
    variable_count: usize,
    name_width: usize,
) -> String {
    let mut source = format!("unit {unit_name};\ninterface\nvar\n");
    for index in 0..variable_count {
        source.push_str(&format!(
            "  {prefix}{index}_{}: Integer;\n",
            "x".repeat(name_width)
        ));
    }
    source.push_str("implementation\nend.\n");
    source
}

pub(super) fn workspace_edit_uris(edit: &Value) -> HashSet<String> {
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

pub(super) fn apply_workspace_edit_to_source(
    source: &str,
    edit: &Value,
    document_uri: &Url,
) -> String {
    let uri = document_uri.to_string();
    let edits = if let Some(changes) = edit["changes"].as_object() {
        changes
            .get(&uri)
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
    } else {
        edit["documentChanges"]
            .as_array()
            .and_then(|changes| {
                changes.iter().find_map(|change| {
                    (change["textDocument"]["uri"] == uri)
                        .then(|| change["edits"].as_array().cloned().unwrap_or_default())
                })
            })
            .unwrap_or_default()
    };
    let mut byte_edits = edits
        .into_iter()
        .map(|edit| {
            let start = text::position_to_offset(
                source,
                serde_json::from_value(edit["range"]["start"].clone())
                    .expect("workspace edit start position"),
            )
            .expect("workspace edit start is a UTF-16 boundary");
            let end = text::position_to_offset(
                source,
                serde_json::from_value(edit["range"]["end"].clone())
                    .expect("workspace edit end position"),
            )
            .expect("workspace edit end is a UTF-16 boundary");
            (
                start,
                end,
                edit["newText"].as_str().unwrap_or_default().to_owned(),
            )
        })
        .collect::<Vec<_>>();
    byte_edits.sort_by(|left, right| right.0.cmp(&left.0).then(right.1.cmp(&left.1)));
    let mut result = source.to_owned();
    for (start, end, new_text) in byte_edits {
        result.replace_range(start..end, &new_text);
    }
    result
}

pub(super) fn diagnostics_for_uri(server: &mut TestServer, expected: &Url) -> Value {
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

pub(super) fn diagnostics_for_uri_until(
    server: &mut TestServer,
    expected: &Url,
    timeout: Duration,
    predicate: impl Fn(&Value) -> bool,
) -> Option<Value> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let publication = server.diagnostic_with_timeout(expected, remaining)?;
        if predicate(&publication) {
            return Some(publication);
        }
    }
    None
}

pub(super) struct SharedOwnerFixture {
    pub(super) shared: PathBuf,
    pub(super) a_main: PathBuf,
    pub(super) a_project: PathBuf,
    pub(super) a_config: PathBuf,
    pub(super) b_main: PathBuf,
    pub(super) b_project: PathBuf,
    pub(super) main_source: String,
    pub(super) shared_source: String,
}

pub(super) fn shared_owner_fixture(root: &Path) -> SharedOwnerFixture {
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

pub(super) struct OpenSharedOwnerFixture {
    pub(super) shared: PathBuf,
    pub(super) shared_source: String,
    pub(super) main_source: String,
    pub(super) a_main: PathBuf,
    pub(super) a_project: PathBuf,
    pub(super) a_config: PathBuf,
    pub(super) b_main: PathBuf,
    pub(super) b_project: PathBuf,
}

pub(super) fn open_shared_owner_fixture(root: &Path) -> OpenSharedOwnerFixture {
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

pub(super) fn assert_exact_rename_edits(
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

pub(super) fn assert_exact_workspace_edit(
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

pub(super) fn standard_workspace() -> (TempDir, PathBuf, PathBuf, String, String) {
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

#[cfg(feature = "test-support")]
pub(super) fn wait_for_file(path: &Path, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if path.exists() {
            return true;
        }
        thread::sleep(Duration::from_millis(5));
    }
    path.exists()
}

#[cfg(feature = "test-support")]
pub(super) fn wait_until_json_bool(
    path: &Path,
    field: &str,
    expected: bool,
    timeout: Duration,
) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Ok(bytes) = fs::read(path)
            && let Ok(value) = serde_json::from_slice::<Value>(&bytes)
            && value[field].as_bool() == Some(expected)
        {
            return true;
        }
        thread::sleep(Duration::from_millis(5));
    }
    false
}
