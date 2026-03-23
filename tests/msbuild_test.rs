use lint4d::discovery::msbuild::parse_msbuild_output;
use std::path::PathBuf;

#[test]
fn parses_key_value_lines() {
    let output = "\
        DCU_OUTPUT=C:\\MyProject\\Win64\\Debug\n\
        UNIT_SEARCH=C:\\MyProject\\lib;C:\\Shared\\units\n\
        PLATFORM=Win64\n\
        CONFIG=Debug\n\
        BDS=C:\\Program Files (x86)\\Embarcadero\\Studio\\23.0\n\
        LIBRARY_PATH=C:\\Program Files (x86)\\Embarcadero\\Studio\\23.0\\lib\\Win64\\release;C:\\Program Files (x86)\\Embarcadero\\Studio\\23.0\\lib\\Win64\\debug\n\
        BROWSING_PATH=\n";

    let base_dir = PathBuf::from("C:\\MyProject");
    let result = parse_msbuild_output(output, &base_dir);

    assert!(result.paths.contains(&PathBuf::from("C:\\MyProject\\Win64\\Debug")));
    assert!(result.paths.contains(&PathBuf::from("C:\\MyProject\\lib")));
    assert!(result.paths.contains(&PathBuf::from("C:\\Shared\\units")));
    assert!(result.paths.iter().any(|p| p.to_string_lossy().contains("lib\\Win64\\release")));
    assert!(result.paths.iter().any(|p| p.to_string_lossy().contains("lib\\Win64\\debug")));
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
    let output = "DCU_OUTPUT=$(DCC_DcuOutput)\nUNIT_SEARCH=C:\\valid\\path\n";
    let result = parse_msbuild_output(output, &PathBuf::from("."));
    // $(DCC_DcuOutput) should be skipped, C:\valid\path should remain
    assert_eq!(result.paths.len(), 1);
    assert_eq!(result.paths[0], PathBuf::from("C:\\valid\\path"));
}

#[test]
fn resolves_relative_paths_against_base_dir() {
    let output = "DCU_OUTPUT=Win64\\Debug\n";
    let base_dir = PathBuf::from("C:\\MyProject");
    let result = parse_msbuild_output(output, &base_dir);
    assert_eq!(result.paths[0], PathBuf::from("C:\\MyProject\\Win64\\Debug"));
}

#[test]
fn deduplicates_paths() {
    let output = "DCU_OUTPUT=C:\\path\\one\nUNIT_SEARCH=C:\\path\\one;C:\\path\\two\n";
    let result = parse_msbuild_output(output, &PathBuf::from("."));
    let count = result.paths.iter().filter(|p| **p == PathBuf::from("C:\\path\\one")).count();
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
        &PathBuf::from("C:\\BDS\\bin\\rsvars.bat").as_path(),
        &PathBuf::from("C:\\tmp\\lint4d.targets").as_path(),
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
        &PathBuf::from("C:\\BDS\\bin\\rsvars.bat").as_path(),
        &PathBuf::from("C:\\tmp\\lint4d.targets").as_path(),
        None,
        None,
    );
    assert!(!cmd.contains("/p:Platform"));
    assert!(!cmd.contains("/p:Config"));
}
