use lint4d::discovery::dproj::parse_dproj;
use lint4d::discovery::dproj::parse_project_version;
use std::path::PathBuf;

#[test]
fn parses_dproj_file_list() {
    let dproj_path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/project/TestProject.dproj");
    let files = parse_dproj(&dproj_path).unwrap();
    assert_eq!(files.len(), 3);
    let names: Vec<String> = files
        .iter()
        .map(|f| f.path.to_string_lossy().to_string())
        .collect();
    assert!(names.iter().any(|n| n.contains("Unit1.pas")));
    assert!(names.iter().any(|n| n.contains("Unit2.pas")));
    assert!(names.iter().any(|n| n.contains("Unit3.pas")));
}

#[test]
fn dproj_nonexistent_returns_error() {
    let result = parse_dproj(&PathBuf::from("nonexistent.dproj"));
    assert!(result.is_err());
}

#[test]
fn parses_project_version_from_dproj() {
    let dproj_path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/project/TestProject.dproj");
    let version = parse_project_version(&dproj_path).unwrap();
    assert_eq!(version, Some("19.5".to_string()));
}

#[test]
fn project_version_returns_none_when_missing() {
    let dir = tempfile::TempDir::new().unwrap();
    let dproj = dir.path().join("Minimal.dproj");
    std::fs::write(&dproj, r#"<?xml version="1.0"?><Project xmlns="http://schemas.microsoft.com/developer/msbuild/2003"><ItemGroup/></Project>"#).unwrap();
    let version = parse_project_version(&dproj).unwrap();
    assert_eq!(version, None);
}
