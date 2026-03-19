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
fn empty_except_flags_empty_handler() {
    let diagnostics = lint_fixture("tests/fixtures/exception/bad_empty_except.pas");
    let matches: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.rule_id == "empty-except")
        .collect();
    assert_eq!(
        matches.len(),
        1,
        "Expected 1 empty-except diagnostic, got {}: {:?}",
        matches.len(),
        matches
    );
}

#[test]
fn empty_except_passes_with_handler() {
    let diagnostics = lint_fixture("tests/fixtures/exception/good_except_handler.pas");
    let matches: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.rule_id == "empty-except")
        .collect();
    assert!(
        matches.is_empty(),
        "Expected no empty-except diagnostics, got: {:?}",
        matches
    );
}

#[test]
fn bare_except_flags_handler_without_on_clause() {
    let diagnostics = lint_fixture("tests/fixtures/exception/bad_bare_except.pas");
    let matches: Vec<_> = diagnostics.iter().filter(|d| d.rule_id == "bare-except").collect();
    assert_eq!(matches.len(), 1);
}

#[test]
fn bare_except_skips_handler_with_raise() {
    let diagnostics = lint_fixture("tests/fixtures/exception/good_bare_except_with_raise.pas");
    let matches: Vec<_> = diagnostics.iter().filter(|d| d.rule_id == "bare-except").collect();
    assert!(matches.is_empty());
}

#[test]
fn bare_except_passes_with_on_clause() {
    let diagnostics = lint_fixture("tests/fixtures/exception/good_except_handler.pas");
    let matches: Vec<_> = diagnostics.iter().filter(|d| d.rule_id == "bare-except").collect();
    assert!(matches.is_empty());
}
