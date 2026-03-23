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
                    "lint4d: warning: skipping unexpanded variable in {}: {}",
                    key, segment
                );
                continue;
            }

            let path = if Path::new(segment).is_relative() {
                base_dir.join(segment)
            } else {
                PathBuf::from(segment)
            };

            if seen.insert(path.clone()) {
                result.paths.push(path);
            }
        }
    }

    result
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
        Ok(p) => p,
        Err(e) => {
            eprintln!("lint4d: warning: cannot resolve dproj path {}: {}", dproj_path.display(), e);
            return Vec::new();
        }
    };

    let base_dir = dproj_abs.parent().unwrap_or_else(|| Path::new("."));

    let targets_xml = generate_targets_xml(&dproj_abs);

    // Write temp .targets file
    // Use system temp dir to avoid polluting the project directory
    let temp_file = match tempfile::Builder::new()
        .prefix("lint4d-")
        .suffix(".targets")
        .tempfile()
    {
        Ok(f) => f,
        Err(e) => {
            eprintln!("lint4d: warning: failed to create temp targets file: {}", e);
            return Vec::new();
        }
    };

    if let Err(e) = std::fs::write(temp_file.path(), &targets_xml) {
        eprintln!("lint4d: warning: failed to write temp targets file: {}", e);
        return Vec::new();
    }

    let cmd_str = build_msbuild_command(
        rsvars_path,
        temp_file.path(),
        platform_override,
        config_override,
    );

    // Invoke MSBuild with 15-second timeout
    let child = std::process::Command::new("cmd")
        .args(["/c", &cmd_str])
        .current_dir(base_dir)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn();

    let child = match child {
        Ok(c) => c,
        Err(e) => {
            eprintln!("lint4d: warning: failed to launch MSBuild: {}", e);
            return Vec::new();
        }
    };

    let output = match wait_with_timeout(child, std::time::Duration::from_secs(15)) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("lint4d: warning: MSBuild invocation failed: {}", e);
            return Vec::new();
        }
    };

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        eprintln!(
            "lint4d: warning: MSBuild exited with status {}. stderr:\n{}",
            output.status, stderr
        );
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

/// Wait for a child process with a timeout. Returns an error if the timeout
/// is exceeded (and kills the process).
#[cfg(target_os = "windows")]
fn wait_with_timeout(
    mut child: std::process::Child,
    timeout: std::time::Duration,
) -> Result<std::process::Output, String> {
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let stdout = child.stdout.take().map_or_else(Vec::new, |mut s| {
                    let mut buf = Vec::new();
                    std::io::Read::read_to_end(&mut s, &mut buf).unwrap_or(0);
                    buf
                });
                let stderr = child.stderr.take().map_or_else(Vec::new, |mut s| {
                    let mut buf = Vec::new();
                    std::io::Read::read_to_end(&mut s, &mut buf).unwrap_or(0);
                    buf
                });
                return Ok(std::process::Output { status, stdout, stderr });
            }
            Ok(None) => {
                if start.elapsed() > timeout {
                    let _ = child.kill();
                    return Err("MSBuild timed out after 15 seconds".to_string());
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            Err(e) => return Err(format!("Failed to wait for MSBuild: {}", e)),
        }
    }
}
