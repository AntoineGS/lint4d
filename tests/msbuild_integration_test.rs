//! Integration tests for MSBuild-based DCU discovery.
//!
//! These tests require a real RAD Studio installation and are skipped
//! by default. Run with: `cargo test -- --ignored`

use lint4d::discovery::bds;
use std::path::PathBuf;

#[test]
#[ignore]
fn finds_bds_root_from_registry() {
    // This test only passes on machines with RAD Studio installed.
    let root = bds::find_any_bds_root();
    assert!(root.is_some(), "Expected to find a BDS installation in the registry");
    let root = root.unwrap();
    assert!(bds::rsvars_bat_path(&root).is_file());
}

#[test]
#[ignore]
fn msbuild_discovery_with_real_dproj() {
    // Point this at a real .dproj on your machine to test end-to-end.
    // Update the path below to a project you have locally.
    let dproj = PathBuf::from("C:\\path\\to\\your\\Project.dproj");
    if !dproj.exists() {
        eprintln!("Skipping: dproj not found at {}", dproj.display());
        return;
    }

    let root = bds::find_any_bds_root().expect("BDS not found");
    let rsvars = bds::rsvars_bat_path(&root);

    let paths = lint4d::discovery::msbuild::discover_dcu_paths_via_msbuild(
        &dproj, &rsvars, None, None,
    );

    eprintln!("Discovered {} DCU paths:", paths.len());
    for p in &paths {
        eprintln!("  {}", p.display());
    }
    assert!(!paths.is_empty(), "Expected at least one DCU path");
}
