use lint4d::discovery::msbuild::parse_msbuild_output;
use std::path::PathBuf;

#[cfg(windows)]
fn native_absolute_root() -> PathBuf {
    PathBuf::from("C:\\")
}

#[cfg(not(windows))]
fn native_absolute_root() -> PathBuf {
    PathBuf::from("/")
}

fn native_absolute_path(parts: &[&str]) -> PathBuf {
    let mut path = native_absolute_root();
    for part in parts {
        path.push(part);
    }
    path
}

fn native_relative_path(parts: &[&str]) -> PathBuf {
    let mut path = PathBuf::new();
    for part in parts {
        path.push(part);
    }
    path
}

#[test]
fn parses_key_value_lines() {
    let project_dir = native_absolute_path(&["MyProject"]);
    let output_dir = project_dir.join("Win64").join("Debug");
    let project_lib = project_dir.join("lib");
    let shared_units = native_absolute_path(&["Shared", "units"]);
    let bds_root = native_absolute_path(&["Program Files (x86)", "Embarcadero", "Studio", "23.0"]);
    let library_release = bds_root.join("lib").join("Win64").join("release");
    let library_debug = bds_root.join("lib").join("Win64").join("debug");
    let output = format!(
        "DCU_OUTPUT={}\n\
         UNIT_SEARCH={};{}\n\
         PLATFORM=Win64\n\
         CONFIG=Debug\n\
         BDS={}\n\
         LIBRARY_PATH={};{}\n\
         BROWSING_PATH=\n",
        output_dir.display(),
        project_lib.display(),
        shared_units.display(),
        bds_root.display(),
        library_release.display(),
        library_debug.display(),
    );

    let result = parse_msbuild_output(&output, &project_dir);

    assert!(result.paths.contains(&output_dir));
    assert!(result.paths.contains(&project_lib));
    assert!(result.paths.contains(&shared_units));
    assert!(result.paths.contains(&library_release));
    assert!(result.paths.contains(&library_debug));
    assert_eq!(result.platform.as_deref(), Some("Win64"));
    assert_eq!(result.config.as_deref(), Some("Debug"));
}

#[test]
fn skips_empty_values() {
    let output = "DCU_OUTPUT=\nUNIT_SEARCH=\nLIBRARY_PATH=\nBROWSING_PATH=\n";
    let result = parse_msbuild_output(output, &PathBuf::from("."));
    assert!(result.paths.is_empty());
}

#[test]
fn skips_unexpanded_variables() {
    let valid_path = native_absolute_path(&["valid", "path"]);
    let output = format!(
        "DCU_OUTPUT=$(DCC_DcuOutput)\nUNIT_SEARCH={}\n",
        valid_path.display()
    );
    let result = parse_msbuild_output(&output, &PathBuf::from("."));
    // $(DCC_DcuOutput) should be skipped, the native absolute path should remain
    assert_eq!(result.paths.len(), 1);
    assert_eq!(result.paths[0], valid_path);
}

#[test]
fn resolves_relative_paths_against_base_dir() {
    let relative_path = native_relative_path(&["Win64", "Debug"]);
    let output = format!("DCU_OUTPUT={}\n", relative_path.display());
    let base_dir = native_absolute_path(&["MyProject"]);
    let result = parse_msbuild_output(&output, &base_dir);
    assert_eq!(result.paths[0], base_dir.join(relative_path));
}

#[cfg(not(windows))]
#[test]
fn resolves_foreign_windows_rooted_and_drive_relative_paths_lexically() {
    let output = "UNIT_SEARCH=\\Shared\\units;D:units\n";
    let base_dir = PathBuf::from(r"C:\MyProject");
    let result = parse_msbuild_output(output, &base_dir);

    assert!(result.paths.contains(&PathBuf::from(r"C:\Shared\units")));
    assert!(result.paths.contains(&PathBuf::from(r"D:units")));
    assert!(
        !result
            .paths
            .contains(&PathBuf::from(r"C:\MyProject\Shared\units"))
    );
    assert!(
        !result
            .paths
            .contains(&PathBuf::from(r"C:\MyProject\D:units"))
    );
}

#[cfg(windows)]
#[test]
fn native_windows_join_preserves_rooted_and_drive_relative_paths() {
    let output = "UNIT_SEARCH=\\Shared\\units;D:units\n";
    let base_dir = PathBuf::from(r"C:\MyProject");
    let result = parse_msbuild_output(output, &base_dir);

    assert!(result.paths.contains(&PathBuf::from(r"C:\Shared\units")));
    assert!(result.paths.contains(&PathBuf::from(r"D:units")));
}

#[test]
fn deduplicates_paths() {
    let first_path = native_absolute_path(&["path", "one"]);
    let second_path = native_absolute_path(&["path", "two"]);
    let output = format!(
        "DCU_OUTPUT={}\nUNIT_SEARCH={};{}\n",
        first_path.display(),
        first_path.display(),
        second_path.display(),
    );
    let result = parse_msbuild_output(&output, &PathBuf::from("."));
    let count = result
        .paths
        .iter()
        .filter(|path| path.as_path() == first_path.as_path())
        .count();
    assert_eq!(count, 1);
}

#[test]
fn ignores_non_path_keys() {
    let output = "PLATFORM=Win64\nCONFIG=Debug\nBDS=C:\\bds\n";
    let result = parse_msbuild_output(output, &PathBuf::from("."));
    // BDS, PLATFORM, CONFIG are metadata, not DCU paths
    assert!(result.paths.is_empty());
    assert_eq!(result.platform.as_deref(), Some("Win64"));
    assert_eq!(result.config.as_deref(), Some("Debug"));
}

#[test]
fn handles_msbuild_noise_lines() {
    let output = "\
        Build started 3/20/2026 10:00:00 AM.\n\
        DCU_OUTPUT=C:\\MyProject\\out\n\
        Build succeeded.\n\
            0 Warning(s)\n\
            0 Error(s)\n";
    let result = parse_msbuild_output(output, &PathBuf::from("."));
    assert_eq!(result.paths.len(), 1);
}

#[test]
fn generate_targets_xml_contains_import_and_target() {
    use lint4d::discovery::msbuild::generate_targets_xml;

    let xml = generate_targets_xml(&PathBuf::from("C:\\MyProject\\Test.dproj"));
    assert!(xml.contains(r#"Import Project="C:\MyProject\Test.dproj""#));
    assert!(xml.contains(r#"Target Name="PrintPaths""#));
    assert!(xml.contains("DCU_OUTPUT=$(DCC_DcuOutput)"));
    assert!(xml.contains("LIBRARY_PATH=$(DelphiLibraryPath)"));
}

#[test]
fn build_msbuild_command_includes_overrides() {
    use lint4d::discovery::msbuild::build_msbuild_command;

    let cmd = build_msbuild_command(
        PathBuf::from("C:\\BDS\\bin\\rsvars.bat").as_path(),
        PathBuf::from("C:\\tmp\\lint4d.targets").as_path(),
        Some("Win64"),
        Some("Release"),
    );
    assert!(cmd.contains("rsvars.bat"));
    assert!(cmd.contains("/p:Platform=Win64"));
    assert!(cmd.contains("/p:Config=Release"));
}

#[test]
fn build_msbuild_command_without_overrides() {
    use lint4d::discovery::msbuild::build_msbuild_command;

    let cmd = build_msbuild_command(
        PathBuf::from("C:\\BDS\\bin\\rsvars.bat").as_path(),
        PathBuf::from("C:\\tmp\\lint4d.targets").as_path(),
        None,
        None,
    );
    assert!(!cmd.contains("/p:Platform"));
    assert!(!cmd.contains("/p:Config"));
}
