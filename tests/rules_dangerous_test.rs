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
fn with_statement_flagged() {
    let diagnostics = lint_fixture("tests/fixtures/dangerous/bad_with.pas");
    let matches: Vec<_> = diagnostics.iter().filter(|d| d.rule_id == "with-statement").collect();
    assert_eq!(matches.len(), 1);
}

#[test]
fn no_with_passes() {
    let diagnostics = lint_fixture("tests/fixtures/dangerous/good_no_with.pas");
    let matches: Vec<_> = diagnostics.iter().filter(|d| d.rule_id == "with-statement").collect();
    assert!(matches.is_empty());
}
