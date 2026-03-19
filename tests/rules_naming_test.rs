use lint4d::config::Config;
use lint4d::engine::{run_lint, FileInfo};
use std::fs;
use std::path::PathBuf;

fn lint_fixture(fixture_path: &str) -> Vec<lint4d::engine::Diagnostic> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(fixture_path);
    let source = fs::read(&path).unwrap();
    let file = FileInfo::new(PathBuf::from(fixture_path));
    let config = Config::from_str("version = 1").unwrap();
    run_lint(&file, &source, &config)
}

#[test]
fn type_prefix_flags_missing_t() {
    let diagnostics = lint_fixture("tests/fixtures/naming/bad_type_prefix.pas");
    let matches: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.rule_id == "type-prefix")
        .collect();
    assert_eq!(
        matches.len(),
        2,
        "Expected 2 type-prefix diagnostics for MyClass and BadRecord, got: {:?}",
        matches
    );
}

#[test]
fn interface_prefix_flags_missing_i() {
    let diagnostics = lint_fixture("tests/fixtures/naming/bad_interface_prefix.pas");
    let matches: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.rule_id == "interface-prefix")
        .collect();
    assert_eq!(matches.len(), 1);
}

#[test]
fn naming_passes_with_correct_prefixes() {
    let diagnostics = lint_fixture("tests/fixtures/naming/good_naming.pas");
    let naming: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.rule_id == "type-prefix" || d.rule_id == "interface-prefix")
        .collect();
    assert!(
        naming.is_empty(),
        "Expected no naming diagnostics, got: {:?}",
        naming
    );
}

#[test]
fn constant_naming_flags_lowercase_constants() {
    let diagnostics = lint_fixture("tests/fixtures/naming/bad_constant_naming.pas");
    let matches: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.rule_id == "constant-naming")
        .collect();
    assert_eq!(
        matches.len(),
        2,
        "Expected 2 constant-naming diagnostics, got: {:?}",
        matches
    );
}

#[test]
fn constant_naming_passes_upper_case() {
    let diagnostics = lint_fixture("tests/fixtures/naming/good_constant_naming.pas");
    let matches: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.rule_id == "constant-naming")
        .collect();
    assert!(
        matches.is_empty(),
        "Expected no constant-naming diagnostics, got: {:?}",
        matches
    );
}

#[test]
fn naming_rules_skipped_for_dpr_files() {
    let path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/naming/bad_type_prefix.pas");
    let source = fs::read(&path).unwrap();
    let file = FileInfo::new(PathBuf::from("Project.dpr"));
    let config = Config::from_str("version = 1").unwrap();
    let diagnostics = run_lint(&file, &source, &config);
    let naming: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.rule_id == "type-prefix" || d.rule_id == "interface-prefix")
        .collect();
    assert!(
        naming.is_empty(),
        "Naming rules should be skipped for .dpr files"
    );
}
