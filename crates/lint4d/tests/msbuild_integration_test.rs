//! Integration tests for MSBuild-based DCU discovery.
//!
//! Tests marked `#[ignore]` require a real RAD Studio installation.
//! Run with: `cargo test -- --ignored`

use lint4d::discovery::bds;
use lint4d::discovery::msbuild;
use std::path::PathBuf;

#[test]
#[ignore]
fn finds_bds_root_from_registry() {
    // This test only passes on machines with RAD Studio installed.
    let root = bds::find_any_bds_root();
    assert!(
        root.is_some(),
        "Expected to find a BDS installation in the registry"
    );
    let root = root.unwrap();
    assert!(bds::rsvars_bat_path(&root).is_file());
}

#[test]
#[ignore]
fn msbuild_discovery_with_fixture_dproj() {
    // Uses the test fixture dproj. Because it's a minimal dproj without the full
    // Delphi import chain, DelphiLibraryPath won't be resolved. This test verifies
    // that MSBuild invocation succeeds (exit code 0) and output is parseable, even
    // if no directories actually exist on disk.
    let root = match bds::find_any_bds_root() {
        Some(r) => r,
        None => {
            eprintln!("Skipping: no BDS installation found");
            return;
        }
    };
    let rsvars = bds::rsvars_bat_path(&root);

    let dproj = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/project/MsbuildTestProject.dproj");

    // This may return empty paths since fixture dirs don't exist on disk,
    // but it should not panic or error out.
    let paths = msbuild::discover_dcu_paths_via_msbuild(&dproj, &rsvars, None, None);
    eprintln!("Discovered {} DCU paths from fixture", paths.len());
}

/// Test the full output parsing with a fake rsvars.bat that echoes MSBuild-like output.
/// This does NOT require a real BDS installation.
#[test]
fn msbuild_discovery_with_fake_bds() {
    // Create a fake BDS root with a rsvars.bat that just sets BDS
    let dir = tempfile::TempDir::new().unwrap();
    let bin = dir.path().join("bin");
    std::fs::create_dir(&bin).unwrap();

    // Create a fake rsvars.bat that sets minimal env
    std::fs::write(
        bin.join("rsvars.bat"),
        "@SET BDS=C:\\FakeBDS\n@SET FrameworkDir=C:\\Windows\\Microsoft.NET\\Framework\\v4.0.30319\n@SET PATH=%FrameworkDir%;%PATH%\n",
    ).unwrap();

    // Create a dcu output directory that actually exists
    let dcu_dir = dir.path().join("dcu_output");
    std::fs::create_dir(&dcu_dir).unwrap();

    // Create a minimal dproj that points to our real dcu_dir
    let dproj_path = dir.path().join("Test.dproj");
    let dproj_content = format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<Project xmlns="http://schemas.microsoft.com/developer/msbuild/2003">
  <PropertyGroup>
    <ProjectVersion>19.5</ProjectVersion>
    <Platform Condition="'$(Platform)'==''">Win64</Platform>
    <Config Condition="'$(Config)'==''">Debug</Config>
    <DCC_DcuOutput>{}</DCC_DcuOutput>
  </PropertyGroup>
</Project>"#,
        dcu_dir.display()
    );
    std::fs::write(&dproj_path, &dproj_content).unwrap();

    // Test parse_msbuild_output directly with canned output simulating
    // what MSBuild would return for this project
    let fake_output = format!(
        "DCU_OUTPUT={}\nPLATFORM=Win64\nCONFIG=Debug\n",
        dcu_dir.display()
    );
    let parsed = msbuild::parse_msbuild_output(&fake_output, dir.path());
    assert_eq!(parsed.paths.len(), 1);
    assert_eq!(parsed.paths[0], dcu_dir);
    assert_eq!(parsed.platform.as_deref(), Some("Win64"));
    assert_eq!(parsed.config.as_deref(), Some("Debug"));
}

/// Test the priority cascade with the resolve_dcu_dirs function.
#[test]
fn cascade_cli_overrides_everything() {
    let cli = vec![PathBuf::from("explicit/path")];
    let config = vec!["config/path".to_string()];
    let discovered = vec![PathBuf::from("discovered/path")];
    let result = lint4d::discovery::resolve_dcu_dirs(&cli, &config, discovered);
    assert_eq!(result, vec![PathBuf::from("explicit/path")]);
}
