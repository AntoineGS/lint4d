#[cfg(target_os = "windows")]
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};

/// Keys in the MSBuild output that contain semicolon-separated DCU paths.
const PATH_KEYS: &[&str] = &["DCU_OUTPUT", "UNIT_SEARCH", "LIBRARY_PATH", "BROWSING_PATH"];

/// Keys that contain metadata (not paths).
const META_KEYS: &[&str] = &["PLATFORM", "CONFIG", "BDS"];

/// Parsed result from MSBuild output.
#[derive(Debug, Default)]
pub struct MsbuildPaths {
    pub paths: Vec<PathBuf>,
    pub platform: Option<String>,
    pub config: Option<String>,
    pub bds: Option<String>,
}

/// Parse the stdout from an MSBuild /t:PrintPaths invocation.
///
/// Extracts KEY=value lines, splits semicolons for path keys, resolves
/// relative paths against `base_dir`, skips unexpanded `$(...)` variables,
/// and deduplicates.
pub fn parse_msbuild_output(output: &str, base_dir: &Path) -> MsbuildPaths {
    let mut result = MsbuildPaths::default();
    let mut seen = std::collections::HashSet::new();

    for line in output.lines() {
        let line = line.trim();
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };

        if META_KEYS.contains(&key) {
            let value = value.trim();
            if !value.is_empty() {
                match key {
                    "PLATFORM" => result.platform = Some(value.to_string()),
                    "CONFIG" => result.config = Some(value.to_string()),
                    "BDS" => result.bds = Some(value.to_string()),
                    _ => {}
                }
            }
            continue;
        }

        if !PATH_KEYS.contains(&key) {
            continue;
        }

        for segment in value.split(';') {
            let segment = segment.trim();
            if segment.is_empty() {
                continue;
            }
            // Skip unexpanded MSBuild variables
            if segment.contains("$(") {
                eprintln!(
                    "warning: skipping unexpanded variable in {}: {}",
                    key, segment
                );
                continue;
            }

            let path = resolve_msbuild_path(segment, base_dir);

            if seen.insert(path.clone()) {
                result.paths.push(path);
            }
        }
    }

    result
}

#[cfg(windows)]
fn resolve_msbuild_path(segment: &str, base_dir: &Path) -> PathBuf {
    base_dir.join(segment)
}

#[cfg(not(windows))]
fn resolve_msbuild_path(segment: &str, base_dir: &Path) -> PathBuf {
    if is_windows_absolute_path(segment) || is_windows_drive_relative_path(segment) {
        return PathBuf::from(segment);
    }
    if is_windows_path(base_dir) {
        return resolve_foreign_windows_path(segment, base_dir);
    }
    if Path::new(segment).is_absolute() || is_windows_rooted_path(segment) {
        return PathBuf::from(segment);
    }
    base_dir.join(segment)
}

#[cfg(not(windows))]
fn resolve_foreign_windows_path(segment: &str, base_dir: &Path) -> PathBuf {
    if is_windows_rooted_path(segment) {
        let segment = segment.trim_start_matches(['\\', '/']);
        if let Some(root) = foreign_windows_root(base_dir) {
            return PathBuf::from(format!("{root}\\{segment}"));
        }
        return PathBuf::from(segment);
    }

    let base = base_dir.to_string_lossy();
    let base = base.trim_end_matches(['\\', '/']);
    PathBuf::from(format!("{base}\\{segment}"))
}

#[cfg(not(windows))]
fn foreign_windows_root(path: &Path) -> Option<String> {
    let path = path.to_string_lossy();
    let bytes = path.as_bytes();
    if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        return Some(path[..2].to_string());
    }
    if let Some(path) = path.strip_prefix("\\\\") {
        let mut components = path
            .split(['\\', '/'])
            .filter(|component| !component.is_empty());
        let server = components.next()?;
        let share = components.next()?;
        return Some(format!("\\\\{server}\\{share}"));
    }
    None
}

#[cfg(not(windows))]
fn is_windows_path(path: &Path) -> bool {
    is_windows_absolute_path(&path.to_string_lossy())
}

#[cfg(not(windows))]
fn is_windows_absolute_path(path: &str) -> bool {
    path.starts_with("\\\\")
        || (path.len() >= 3
            && path.as_bytes()[0].is_ascii_alphabetic()
            && path.as_bytes()[1] == b':'
            && matches!(path.as_bytes()[2], b'\\' | b'/'))
}

#[cfg(not(windows))]
fn is_windows_drive_relative_path(path: &str) -> bool {
    path.len() >= 2
        && path.as_bytes()[0].is_ascii_alphabetic()
        && path.as_bytes()[1] == b':'
        && !matches!(path.as_bytes().get(2), Some(b'\\' | b'/'))
}

#[cfg(not(windows))]
fn is_windows_rooted_path(path: &str) -> bool {
    path.starts_with(['\\', '/'])
}

/// Generate the MSBuild `.targets` XML content that imports a dproj and
/// prints resolved properties.
pub fn generate_targets_xml(dproj_absolute_path: &Path) -> String {
    format!(
        r#"<Project xmlns="http://schemas.microsoft.com/developer/msbuild/2003">
  <Import Project="{}"/>
  <Target Name="PrintPaths">
    <Message Text="DCU_OUTPUT=$(DCC_DcuOutput)" Importance="High"/>
    <Message Text="UNIT_SEARCH=$(DCC_UnitSearchPath)" Importance="High"/>
    <Message Text="PLATFORM=$(Platform)" Importance="High"/>
    <Message Text="CONFIG=$(Config)" Importance="High"/>
    <Message Text="BDS=$(BDS)" Importance="High"/>
    <Message Text="LIBRARY_PATH=$(DelphiLibraryPath)" Importance="High"/>
    <Message Text="BROWSING_PATH=$(DelphiBrowsingPath)" Importance="High"/>
  </Target>
</Project>"#,
        dproj_absolute_path.display()
    )
}

/// Build the `cmd /c` command string for invoking MSBuild.
///
/// Accepts optional platform and build-config overrides that are forwarded
/// as `/p:Platform=...` and `/p:Config=...`.
pub fn build_msbuild_command(
    rsvars_path: &Path,
    targets_path: &Path,
    platform_override: Option<&str>,
    config_override: Option<&str>,
) -> String {
    let mut cmd = format!(
        r#"call "{}" && msbuild "{}" /t:PrintPaths /nologo /v:minimal"#,
        rsvars_path.display(),
        targets_path.display(),
    );
    if let Some(platform) = platform_override {
        cmd.push_str(&format!(" /p:Platform={}", platform));
    }
    if let Some(config) = config_override {
        cmd.push_str(&format!(" /p:Config={}", config));
    }
    cmd
}

/// Run MSBuild discovery for a dproj file.
///
/// Creates a temp `.targets` file, invokes MSBuild via `rsvars.bat`, parses
/// the output, and returns discovered DCU paths. Returns an empty list on
/// any failure (with warnings on stderr).
///
/// Only available on Windows — on other platforms this is a no-op.
#[cfg(target_os = "windows")]
pub fn discover_dcu_paths_via_msbuild(
    dproj_path: &Path,
    rsvars_path: &Path,
    platform_override: Option<&str>,
    config_override: Option<&str>,
) -> Vec<PathBuf> {
    let dproj_abs = match std::fs::canonicalize(dproj_path) {
        Ok(p) => strip_unc_prefix(p),
        Err(e) => {
            eprintln!(
                "warning: cannot resolve dproj path {}: {}",
                dproj_path.display(),
                e
            );
            return Vec::new();
        }
    };

    let base_dir = dproj_abs.parent().unwrap_or_else(|| Path::new("."));

    let targets_xml = generate_targets_xml(&dproj_abs);

    // Write temp .targets file to system temp dir.
    // Use a plain file (not tempfile::NamedTempFile) because NamedTempFile
    // holds the file handle open, which prevents MSBuild from reading it.
    let temp_path = std::env::temp_dir().join(format!("lint4d-{}.targets", std::process::id()));
    if let Err(e) = std::fs::write(&temp_path, &targets_xml) {
        eprintln!("warning: failed to write temp targets file: {}", e);
        return Vec::new();
    }

    let cmd_str =
        build_msbuild_command(rsvars_path, &temp_path, platform_override, config_override);

    // Invoke MSBuild with 15-second timeout.
    // Use raw_arg to avoid Windows double-quoting the command string,
    // which breaks `call "path with spaces"` inside cmd /c.
    let child = std::process::Command::new("cmd")
        .raw_arg(format!("/c {}", cmd_str))
        .current_dir(base_dir)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn();

    let child = match child {
        Ok(c) => c,
        Err(e) => {
            let _ = std::fs::remove_file(&temp_path);
            eprintln!("warning: failed to launch MSBuild: {}", e);
            return Vec::new();
        }
    };

    let (output, stdout_truncated) =
        match wait_with_timeout(child, std::time::Duration::from_secs(15)) {
            Ok(o) => o,
            Err(e) => {
                let _ = std::fs::remove_file(&temp_path);
                eprintln!("warning: MSBuild invocation failed: {}", e);
                return Vec::new();
            }
        };

    // Clean up temp file after MSBuild completes
    let _ = std::fs::remove_file(&temp_path);

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        eprintln!(
            "warning: MSBuild exited with status {}: {}",
            output.status,
            stderr.trim()
        );
        return Vec::new();
    }

    // Like every other MSBuild failure, a truncated listing yields no paths
    // rather than a silently partial list.
    if stdout_truncated {
        eprintln!("warning: MSBuild output exceeded the capture limit; ignoring its paths");
        return Vec::new();
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed = parse_msbuild_output(&stdout, base_dir);

    // Filter to directories that actually exist
    parsed.paths.into_iter().filter(|p| p.is_dir()).collect()
}

#[cfg(not(target_os = "windows"))]
pub fn discover_dcu_paths_via_msbuild(
    _dproj_path: &Path,
    _rsvars_path: &Path,
    _platform_override: Option<&str>,
    _config_override: Option<&str>,
) -> Vec<PathBuf> {
    Vec::new()
}

/// Strip the `\\?\` extended-length prefix that `std::fs::canonicalize`
/// adds on Windows. MSBuild and other tools cannot handle this prefix.
#[cfg(target_os = "windows")]
fn strip_unc_prefix(path: PathBuf) -> PathBuf {
    let s = path.to_string_lossy();
    if let Some(stripped) = s.strip_prefix(r"\\?\") {
        PathBuf::from(stripped)
    } else {
        path
    }
}

/// Captured output per pipe; anything beyond this is read and discarded.
const MAX_CAPTURED_PIPE_BYTES: usize = 1024 * 1024;

/// Bytes captured from a pipe and whether more were read and discarded.
type DrainedPipe = (Vec<u8>, bool);

/// Read `pipe` to EOF, keeping at most `MAX_CAPTURED_PIPE_BYTES` so the child
/// never blocks on a full pipe.
fn drain_pipe(
    mut pipe: impl std::io::Read + Send + 'static,
) -> std::thread::JoinHandle<DrainedPipe> {
    std::thread::spawn(move || {
        let mut captured = Vec::new();
        let mut truncated = false;
        let mut chunk = [0u8; 8192];
        loop {
            match pipe.read(&mut chunk) {
                Ok(0) | Err(_) => return (captured, truncated),
                Ok(read) => {
                    let room = MAX_CAPTURED_PIPE_BYTES.saturating_sub(captured.len());
                    truncated |= read > room;
                    captured.extend_from_slice(&chunk[..read.min(room)]);
                }
            }
        }
    })
}

/// Wait for a child process with a timeout. Returns an error if the timeout
/// is exceeded (and kills and reaps the process).
///
/// Stdout and stderr are drained on reader threads from launch, so a child
/// that fills a pipe is not mistaken for a hung one. The flag reports that
/// stdout exceeded `MAX_CAPTURED_PIPE_BYTES` and was truncated.
#[cfg_attr(not(any(windows, test)), allow(dead_code))]
fn wait_with_timeout(
    mut child: std::process::Child,
    timeout: std::time::Duration,
) -> Result<(std::process::Output, bool), String> {
    let stdout = child.stdout.take().map(drain_pipe);
    let stderr = child.stderr.take().map(drain_pipe);
    let join = |reader: Option<std::thread::JoinHandle<DrainedPipe>>| {
        reader.map_or_else(DrainedPipe::default, |reader| {
            reader.join().unwrap_or_default()
        })
    };
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let (stdout, stdout_truncated) = join(stdout);
                let (stderr, _) = join(stderr);
                return Ok((
                    std::process::Output {
                        status,
                        stdout,
                        stderr,
                    },
                    stdout_truncated,
                ));
            }
            Ok(None) => {
                if start.elapsed() > timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!(
                        "MSBuild timed out after {} seconds",
                        timeout.as_secs()
                    ));
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            Err(e) => return Err(format!("Failed to wait for MSBuild: {}", e)),
        }
    }
}

#[cfg(all(test, unix))]
mod wait_with_timeout_tests {
    use super::wait_with_timeout;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    fn spawn_sh(script: &str) -> std::process::Child {
        Command::new("sh")
            .args(["-c", script])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("sh should spawn")
    }

    #[test]
    fn child_writing_more_than_a_pipe_buffer_completes_without_timeout() {
        let child = spawn_sh("head -c 4194304 /dev/zero; head -c 2097152 /dev/zero >&2");
        let started = Instant::now();
        let (output, stdout_truncated) = wait_with_timeout(child, Duration::from_secs(10))
            .expect("a child that fills its pipes must not time out");
        assert!(stdout_truncated);
        assert!(output.status.success());
        assert!(started.elapsed() < Duration::from_secs(8));
        assert_eq!(output.stdout.len(), 1024 * 1024);
        assert_eq!(output.stderr.len(), 1024 * 1024);
    }

    #[test]
    fn timeout_kills_and_returns_an_error() {
        let child = spawn_sh("exec sleep 30");
        let started = Instant::now();
        let error = wait_with_timeout(child, Duration::from_millis(300))
            .expect_err("a hung child must time out");
        assert!(error.contains("timed out"));
        assert!(started.elapsed() < Duration::from_secs(5));
    }
}
