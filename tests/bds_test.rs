use lint4d::discovery::bds::{bds_version_for_project, BdsInfo};

#[test]
fn maps_project_version_19_5_to_bds_23() {
    let info = bds_version_for_project("19.5").unwrap();
    assert_eq!(info.bds_version, "23.0");
    assert_eq!(info.product_name, "Delphi 12 Athens");
}

#[test]
fn maps_project_version_19_4_to_bds_22() {
    let info = bds_version_for_project("19.4").unwrap();
    assert_eq!(info.bds_version, "22.0");
}

#[test]
fn maps_project_version_19_3_to_bds_21() {
    let info = bds_version_for_project("19.3").unwrap();
    assert_eq!(info.bds_version, "21.0");
}

#[test]
fn maps_project_version_18_8_to_bds_20() {
    let info = bds_version_for_project("18.8").unwrap();
    assert_eq!(info.bds_version, "20.0");
}

#[test]
fn maps_project_version_18_5_to_bds_19_range() {
    let info = bds_version_for_project("18.5").unwrap();
    assert_eq!(info.bds_version, "19.0");
}

#[test]
fn unknown_project_version_returns_none() {
    assert!(bds_version_for_project("99.0").is_none());
}

#[test]
fn rsvars_path_construction() {
    use std::path::PathBuf;
    use lint4d::discovery::bds::rsvars_bat_path;

    let bds_root = PathBuf::from("C:/Program Files (x86)/Embarcadero/Studio/23.0");
    let rsvars = rsvars_bat_path(&bds_root);
    assert_eq!(rsvars, PathBuf::from("C:/Program Files (x86)/Embarcadero/Studio/23.0/bin/rsvars.bat"));
}

#[test]
fn discover_bds_root_uses_explicit_path() {
    use lint4d::discovery::bds::{discover_bds_root, rsvars_bat_path};

    // Create a fake BDS root with bin/rsvars.bat
    let dir = tempfile::TempDir::new().unwrap();
    let bin = dir.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    std::fs::write(bin.join("rsvars.bat"), "@echo off").unwrap();

    let result = discover_bds_root(Some(dir.path()), None);
    assert_eq!(result, Some(dir.path().to_path_buf()));
}

#[test]
fn discover_bds_root_rejects_invalid_explicit_path() {
    use lint4d::discovery::bds::discover_bds_root;

    let dir = tempfile::TempDir::new().unwrap();
    // No bin/rsvars.bat here
    let result = discover_bds_root(Some(dir.path()), None);
    assert_eq!(result, None);
}
